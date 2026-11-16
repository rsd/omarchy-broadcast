//! NVIDIA Maxine Video Effects backend
//! (`docs/adr/0012-video-effects-and-maxine-backend.md`).
//!
//! The user-installed SDK is opened at run time with `dlopen`, never linked
//! and never vendored. [`MaxineFactory`] finds it in the SDK root
//! ([`effective_root`]: `$MAXINE_VFX_ROOT`; with the variable unset or
//! empty, the root the process configured ([`set_configured_root`], the
//! daemon's `[effect] maxine_root`); else [`DEFAULT_ROOT`] when that
//! directory exists; else the library search path) and reports what to do
//! when it cannot;
//! [`MaxineBackend`] implements [`InferenceBackend`] for `none`, `blur`
//! and `replace`, each with any of the stages of [`LoadRequest::stages`]
//! (the `denoise` stage and the `relight` stage, [`stages`]); `none` needs
//! at least one.
//! [`feature_availability`] says which SDK features are installed, from the
//! filesystem alone, and [`stage_availability`] (or, from the features,
//! [`stages_availability`]) whether a stage can be offered.
//!
//! Per frame, all on the GPU and on the caller's stream, with no host copy,
//! in the stage order of the seam (`obc_backend`, "Effects, stages and
//! their order"):
//!
//! A pre-mask stage runs before the mask and a post-mask one after it
//! ([`needs_mask`]), whatever their order in the request.
//!
//! 1. NV12 to BGR (`NvCVImage_TransferFromYUV`, matrix and range from the
//!    request's [`ColourSpec`]): into the frame image, or, with a pre-mask
//!    stage, into the first such stage's own input image;
//! 2. the pre-mask stages. The denoise stage: the `Denoising` effect
//!    (temporal state, the strength fixed when the factory is made) from
//!    that image into the frame image, so every later step reads the
//!    denoised frame;
//! 3. the `GreenScreen` effect (quality mode, temporal state, CUDA graph)
//!    makes an 8-bit alpha mask, when the effect or a stage needs it;
//! 4. a CUDA kernel ([`obc_gpu::Lut8`]) hardens the mask through a fixed
//!    table into a second mask ([`matte`]): the raw mask lets a faint copy
//!    of the room through, so nothing but the hardened mask is ever used;
//! 5. the post-mask stages. The relight stage: the `Relighting` effect on
//!    the frame image and the hardened mask, lit by a lighting environment
//!    made in code ([`hdri`]), into an image of its own whose background
//!    pixels are not valid. Its direction (`AnglePan`) and foreground gain
//!    (`FgGain`) are set on the loaded effect when a frame asks for others,
//!    and a new light binds a new lighting environment to it, live, with no
//!    new load. The effect processes the environment inside
//!    `NvVFX_SetImage` (and at `NvVFX_Load` while one is bound), in work
//!    that is not ordered with a non-blocking stream such as the frame
//!    stream: on one it reads the environment before it is in place and
//!    lights with nothing (a dark foreground that ignores the direction,
//!    for the binding's life). So the stage owns a blocking stream
//!    (`Relight::blocking`), bound from before the environment is set until
//!    the load (or the new binding) has finished and that stream is
//!    synchronised; the frame stream is bound for every run;
//! 6. `blur`: the `BackgroundBlur` effect on frame and hardened mask;
//!    `replace`: `NvCVImage_Composite` of the frame over the background
//!    through the hardened mask. With the relight stage the foreground is
//!    the relit image instead of the frame: `replace` composites it over the
//!    background, `none` over the frame image (the denoised camera frame),
//!    and `blur` over the blur's output, into the frame image (which nothing
//!    reads any more by then), so no invalid pixel ever reaches the output
//!    and no extra image is needed;
//! 7. BGR to NV12 into the destination (`NvCVImage_TransferToYUV`).
//!
//! A replace composites over its load-time background image, or over the
//! per-frame background in force
//! ([`InferenceBackend::set_background_frame`], for a video background):
//! each such NV12 frame is converted as the camera frames are (step 1),
//! on the same stream, into one of two images of its own, so the load-time
//! image stays for a return to it, a failure leaves the previous picture,
//! and stream order alone keeps a queued composite from reading a
//! half-written one ([`BACKGROUND_STEP`] is its time).
//!
//! Each stage has a slot of its own in the loaded chain, a branch in the
//! validation, in `configure` and in the frame, and is released with the
//! chain.
//!
//! Without the denoise stage step 2 does not exist: the conversion writes
//! the frame image directly, as before the stage was added. Without the
//! relight stage step 5 does not exist and the composite is the effect's
//! alone. With `none` and no relight stage steps 3 to 6 do not exist: the
//! denoised frame goes straight back to NV12, and no green-screen model is
//! loaded. With `none` and the relight stage the mask exists for the stage
//! alone, so any failure of the mask, the stage or its composite is the
//! stage's ([`BackendError::Stage`]), and fails open.
//!
//! Failing to set the kernel up fails the load ([`BackendError::Load`]);
//! failing to launch it fails the frame ([`BackendError::Run`]). Either way
//! the caller shows its failure output; no path falls back to the raw mask.
//! A stage alone failing is [`BackendError::Stage`]: a load leaves nothing
//! of the request in force, a frame drops the stage (and, under `none`, the
//! mask that only the relight stage needed).
//!
//! With a root, the backend loads the SDK's bundled libraries from it itself,
//! by absolute path and dependencies first, before it opens the SDK
//! (`preload`): the process needs no `LD_LIBRARY_PATH`. Without a root the
//! library search path alone is used, so the SDK's library directories must
//! then be on `LD_LIBRARY_PATH` when the process starts; a root without any
//! SDK library is treated the same way. One SDK per process: the place the
//! first SDK library is loaded from (the first probe or load of an effect or
//! of the denoise stage) is the process's for good, even when that open
//! fails, because a loaded library is never unloaded. A root is recorded as
//! the directory it resolves to then, and the search path as the directory
//! the loader found the SDK in. An open that would take the SDK from
//! anywhere else is refused until the process restarts ([`root_conflict`]).
//! On a distribution the SDK does not list, the process also needs the
//! os-release override from the moment it starts: `omarchy-broadcastd`
//! applies that of the `maxine-vfx-sdk-local` package to itself when it
//! starts (`docs/setup/maxine.md`, sections 3 and 4).

use std::cell::RefCell;
use std::ffi::{CString, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicU16, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use obc_backend::{
    Availability, BackendError, BackendFactory, BackgroundFrame, ColourMatrix, ColourSpec,
    DeviceNv12, EffectKind, FrameParams, GpuContext, HostBgr, InferenceBackend, LoadReport,
    LoadRequest, Params, StageId, StageList, StageTimings,
};
use obc_gpu::{CuStream, DeviceBuffer, Event, GpuError, LUT8_BLOCK, Lut8};

mod features;
mod ffi;
pub mod hdri;
pub mod matte;
mod preload;
pub mod stages;
pub mod truehdr;
pub mod vfg;
pub mod vsr;

pub use features::{Feature, feature_availability, feature_dir};
pub use stages::{
    BACKGROUND_CHAIR, GREEN_SCREEN_MODE_CHAIR_BACKGROUND, GREEN_SCREEN_MODE_CHAIR_FOREGROUND,
    RELIGHT_BRIGHTNESS, RELIGHT_DIRECTION, RELIGHT_LIGHT, RelightLight, RelightParams, STAGES,
    any_feature_suffices, check_stage_params, green_screen_mode, needs_mask, relight_frame,
    stage_availability_of, stage_features, stages_availability,
};

/// Whether `stage` can be offered from the SDK tree at `root`: every
/// feature it needs ([`stage_features`]: for the relight stage, the
/// relighting and the green screen that makes its matte; for the enhance
/// stage, either of its methods) is installed, as
/// [`feature_availability`] reports it ([`stage_availability_of`]). Reads
/// directories only.
pub fn stage_availability(
    root: &Path,
    compute_capability: Option<(u32, u32)>,
    stage: &str,
) -> Availability {
    stage_availability_of(&feature_availability(root, compute_capability), stage)
}

use ffi::{NvCvImage, NvCvStatus, Sdk, StateHandle, VfxHandle};

/// The environment variable that names the SDK root (the directory with
/// `lib/`, `features/` and `external/`).
pub const ROOT_ENV: &str = "MAXINE_VFX_ROOT";

/// The SDK root used when [`ROOT_ENV`] is unset or empty, no root is
/// configured ([`set_configured_root`]) and this directory exists: where
/// the `maxine-vfx-sdk-local` package (built from omarchy-nvidia-sdks)
/// installs the SDK tree.
pub const DEFAULT_ROOT: &str = "/opt/nvidia/maxine/videofx";

/// The root the process configured ([`set_configured_root`]).
static CONFIGURED_ROOT: RwLock<Option<PathBuf>> = RwLock::new(None);

/// Sets the SDK root of this process's configuration (the daemon's
/// `[effect] maxine_root`; `None`: none configured), which
/// [`MaxineFactory::from_env`] uses when [`ROOT_ENV`] is unset or empty.
/// Process-wide, like the SDK itself: factories made afterwards use it,
/// factories made before keep the root they have. Once the SDK has been
/// loaded from one place, a factory whose root leads anywhere else fails its
/// probe and loads until the process restarts ([`root_conflict`]).
pub fn set_configured_root(root: Option<PathBuf>) {
    *CONFIGURED_ROOT.write().unwrap_or_else(|p| p.into_inner()) = root;
}

/// The root set with [`set_configured_root`].
pub fn configured_root() -> Option<PathBuf> {
    CONFIGURED_ROOT
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
}

/// The SDK root [`ROOT_ENV`] gives, when it is set and non-empty: it takes
/// precedence over the configured root ([`effective_root`]).
pub fn env_root() -> Option<PathBuf> {
    std::env::var_os(ROOT_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The SDK root [`MaxineFactory::from_env`] uses now: [`ROOT_ENV`] when set
/// and non-empty, else the configured root ([`set_configured_root`]), else
/// [`DEFAULT_ROOT`] when it is a directory, else `None` (the library search
/// path only).
pub fn effective_root() -> Option<PathBuf> {
    resolve_root(
        std::env::var_os(ROOT_ENV).as_deref(),
        configured_root().as_deref(),
        Path::new(DEFAULT_ROOT),
    )
}

/// The SDK root this process has loaded its SDK from, when it has loaded
/// one from a root (or from a root's `lib` on the library search path);
/// `None` before the first SDK library is loaded. Once loaded, that SDK is
/// the process's until it restarts ([`root_conflict`]), whatever root is
/// configured afterwards. Cheap: opens nothing.
pub fn loaded_root() -> Option<PathBuf> {
    ffi::loaded_root()
}

/// The SDK root whose features this process can use: the one it loaded
/// ([`loaded_root`]) once it has loaded the SDK, else the one in force
/// ([`effective_root`]). Opens nothing.
pub fn usable_root() -> Option<PathBuf> {
    pick_usable_root(loaded_root(), effective_root)
}

fn pick_usable_root(
    loaded: Option<PathBuf>,
    effective: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    loaded.or_else(effective)
}

/// Why the SDK cannot be opened from `root` in this process: it is already
/// loaded from elsewhere, which only a restart can change. `None` when no
/// SDK library is loaded yet, or the SDK `root` leads to is the one loaded
/// (the same directory; a root without SDK libraries leads to the library
/// search path). Cheap: opens nothing.
pub fn root_conflict(root: Option<&Path>) -> Option<String> {
    ffi::root_conflict(root)
}

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
    /// Frames (not the warm-up run inside `load`) overwrite the hardened
    /// mask, after the kernel and before anything reads it, with a fixed
    /// split ([`split_mask_value`]): background (0) left of a soft ramp of
    /// [`SPLIT_MASK_RAMP`] columns centred on the frame, foreground (255)
    /// right of it. A test can then check what an effect does on each side
    /// and on the ramp whatever the model sees.
    SplitMask,
}

/// The width in columns of the soft ramp of [`MaskFault::SplitMask`].
#[doc(hidden)]
pub const SPLIT_MASK_RAMP: u32 = 64;

/// The hardened mask value [`MaskFault::SplitMask`] gives column `x` of a
/// frame `width` columns wide: 0 left of the ramp, 255 right of it, and on
/// it a rise strictly between the two.
#[doc(hidden)]
pub fn split_mask_value(x: u32, width: u32) -> u8 {
    let start = (width / 2).saturating_sub(SPLIT_MASK_RAMP / 2);
    match x.checked_sub(start) {
        None => 0,
        Some(i) if i < SPLIT_MASK_RAMP => ((i + 1) * 255 / (SPLIT_MASK_RAMP + 1)) as u8,
        Some(_) => 255,
    }
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
            Some(MaskFault::SplitMask) => 3,
        };
        self.0.store(v, Ordering::SeqCst);
    }

    /// The armed fault. Relaxed: the switch publishes no other data, and a
    /// frame that reads the old value just before a change is fine.
    pub fn get(&self) -> Option<MaskFault> {
        match self.0.load(Ordering::Relaxed) {
            1 => Some(MaskFault::ModuleLoad),
            2 => Some(MaskFault::Launch),
            3 => Some(MaskFault::SplitMask),
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

/// A failure a test can force on the relight stage, as [`DenoiseFault`]
/// does on the denoise stage: only Rust code can set one
/// ([`RelightFaultSwitch`]).
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelightFault {
    /// Loads of the stage create the effect with a selector the SDK does
    /// not know, so `NvVFX_CreateEffect` fails and so does `load`.
    Load,
    /// Frames (not the warm-up run inside `load`) report a failed relight
    /// run instead of calling the SDK, so `process` fails and drops the
    /// stage.
    Run,
    /// A new light on the loaded stage (a live rebind in `load`) fails
    /// before anything is bound, so `load` fails with the stage's error and
    /// drops the stage.
    Rebind,
}

/// The relight fault every backend of a factory reads, shared like
/// [`MaskFaultSwitch`]. Disarmed by default.
#[doc(hidden)]
#[derive(Debug, Clone, Default)]
pub struct RelightFaultSwitch(Arc<AtomicU8>);

impl RelightFaultSwitch {
    /// Arms `fault` (`None`: disarms).
    pub fn set(&self, fault: Option<RelightFault>) {
        let v = match fault {
            None => 0,
            Some(RelightFault::Load) => 1,
            Some(RelightFault::Run) => 2,
            Some(RelightFault::Rebind) => 3,
        };
        self.0.store(v, Ordering::SeqCst);
    }

    /// The armed fault (relaxed, as [`MaskFaultSwitch::get`]).
    pub fn get(&self) -> Option<RelightFault> {
        match self.0.load(Ordering::Relaxed) {
            1 => Some(RelightFault::Load),
            2 => Some(RelightFault::Run),
            3 => Some(RelightFault::Rebind),
            _ => None,
        }
    }
}

/// Finds the Maxine SDK and creates [`MaxineBackend`] instances.
#[derive(Debug, Clone)]
pub struct MaxineFactory {
    root: Option<PathBuf>,
    names: ffi::LibNames,
    faults: MaskFaultSwitch,
    denoise_faults: DenoiseFaultSwitch,
    relight_faults: RelightFaultSwitch,
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

/// The SDK root from the [`ROOT_ENV`] value and the configured root: the
/// variable when set and non-empty; else `configured`; else `default` when
/// it is a directory; else `None` (the library search path only).
fn resolve_root(
    value: Option<&std::ffi::OsStr>,
    configured: Option<&Path>,
    default: &Path,
) -> Option<PathBuf> {
    match value.filter(|v| !v.is_empty()) {
        Some(v) => Some(PathBuf::from(v)),
        None => configured
            .map(Path::to_path_buf)
            .or_else(|| default.is_dir().then(|| default.to_path_buf())),
    }
}

impl MaxineFactory {
    /// Uses the SDK root in force now ([`effective_root`]): `$MAXINE_VFX_ROOT`;
    /// unset or empty, the configured root ([`set_configured_root`]); else
    /// [`DEFAULT_ROOT`] when that directory exists; else the library search
    /// path only. With a root, the SDK's libraries are loaded from it by
    /// absolute path, so `LD_LIBRARY_PATH` is not needed.
    ///
    /// For development, [`DENOISE_STRENGTH_ENV`] (`0` or `1`) overrides
    /// [`DEFAULT_DENOISE_STRENGTH`]. Any other value makes every load that
    /// asks for the denoise stage fail with [`BackendError::Stage`] naming
    /// the variable; loads without it are unaffected.
    pub fn from_env() -> MaxineFactory {
        let mut f = MaxineFactory::with_root(effective_root());
        f.denoise_strength =
            parse_denoise_strength(std::env::var_os(DENOISE_STRENGTH_ENV).as_deref());
        f
    }

    /// Uses `root` as the SDK root (`None`: the library search path only),
    /// whatever the environment and the configured root say.
    pub fn with_root(root: Option<PathBuf>) -> MaxineFactory {
        MaxineFactory {
            root,
            names: ffi::SDK_LIBS,
            faults: MaskFaultSwitch::default(),
            denoise_faults: DenoiseFaultSwitch::default(),
            relight_faults: RelightFaultSwitch::default(),
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
    /// or the [`BackendError::Stage`] such a load would fail with.
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

    /// Makes every backend this factory creates read `switch` for its relight
    /// stage (tests only).
    #[doc(hidden)]
    pub fn with_relight_faults(mut self, switch: RelightFaultSwitch) -> MaxineFactory {
        self.relight_faults = switch;
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
            relight_faults: self.relight_faults.clone(),
            denoise_strength: self.denoise_strength.clone(),
            loaded: None,
            last_timings: RefCell::new(None),
            model_loads: ModelLoads::default(),
        })
    }

    fn sdk(&self) -> Result<&'static Sdk, BackendError> {
        ffi::open(self.root.as_deref(), &self.names).map_err(BackendError::Unavailable)
    }
}

/// The error of a load that asks for the denoise stage when the strength
/// override is unusable.
fn strength_err(message: String) -> BackendError {
    BackendError::Stage {
        stage: StageId::DENOISE,
        code: ffi::NVCV_ERR_PARAMETER,
        message,
    }
}

fn model_dir(sdk: &Sdk) -> Result<PathBuf, BackendError> {
    let dir = sdk.model_dir.clone().ok_or_else(|| {
        BackendError::Unavailable(
            "the Maxine model directory could not be located (the loader did not report where \
             libVideoFX.so is); set [effect] maxine_root or MAXINE_VFX_ROOT to the SDK \
             directory (docs/setup/maxine.md, section 4)"
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

    /// The relight stage's light, direction and brightness
    /// ([`check_stage_params`]).
    fn check_params(&self, stage: &StageId, params: &Params, live: &Params) -> Result<(), String> {
        check_stage_params(stage.as_str(), params, live)
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
        ffi::NVCV_ERR_UNIMPLEMENTED if call.starts_with("NvVFX_CreateEffect") => {
            let lib = feature_library(call);
            match &sdk.root {
                Some(root) => format!(
                    "{base}: the SDK could not open the effect's feature library ({lib}). It \
                     is loaded from the SDK root's features/*/lib directories: install the \
                     feature into {} (docs/setup/maxine.md, section 4){}",
                    root.display(),
                    sdk.preload_failures
                        .as_ref()
                        .map(|f| format!(". SDK libraries that could not be loaded: {f}"))
                        .unwrap_or_default()
                ),
                None => format!(
                    "{base}: the SDK could not open the effect's feature library ({lib}): the \
                     SDK was loaded from the library search path{}, not from an SDK root, so \
                     the feature is looked up there only. Set [effect] maxine_root or \
                     MAXINE_VFX_ROOT to the SDK directory, or put its features/*/lib \
                     directories on LD_LIBRARY_PATH before the process starts \
                     (docs/setup/maxine.md, section 4)",
                    sdk.lib_dir
                        .as_ref()
                        .map(|d| format!(" ({})", d.display()))
                        .unwrap_or_default()
                ),
            }
        }
        ffi::NVCV_ERR_CUDA_KERNEL if call.starts_with("NvVFX_Load") => format!(
            "{base}: the SDK rejected this distribution (its model loader reads \
             /etc/os-release). Install the maxine-vfx-sdk-local package: the daemon applies \
             its os-release override when it starts and logs it, unless OBC_NO_SDK_ENV is set \
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
    } else if call.contains("(Relighting)") {
        "libnvVFXRelighting.so"
    } else {
        "libnvVFXGreenScreen.so / libnvVFXBackgroundBlur.so / libnvVFXDenoising.so / \
         libnvVFXRelighting.so"
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

    /// The value of the `unsigned int` parameter `p`.
    fn get_u32(&self, p: &std::ffi::CStr) -> Result<u32, NvCvStatus> {
        let mut v = 0u32;
        // SAFETY: live handle; static NUL-terminated parameter name; `v` is
        // a valid out-pointer.
        let rc = unsafe { (self.sdk.NvVFX_GetU32)(self.handle, p.as_ptr(), &mut v) };
        if rc == ffi::NVCV_SUCCESS {
            Ok(v)
        } else {
            Err(rc)
        }
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

/// An image allocated by the SDK (`NvCVImage_Alloc`), boxed so its
/// address is stable for `NvVFX_SetImage`, and freed on drop. In GPU
/// memory, except the lighting environments of [`GpuImage::host_hdri`].
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
        Self::alloc_component(sdk, width, height, format, ffi::NVCV_U8, alignment)
    }

    /// A tightly pitched image of packed RGB10A2 words (`NVCV_RGB10A2`,
    /// `NVCV_P32`), as the TrueHDR effect writes.
    fn alloc_rgb10a2_tight(
        sdk: &'static Sdk,
        width: u32,
        height: u32,
    ) -> Result<GpuImage, BackendError> {
        Self::alloc_component(
            sdk,
            width,
            height,
            ffi::NVCV_RGB10A2,
            ffi::NVCV_P32,
            ffi::NVCV_TIGHT_PITCH,
        )
    }

    fn alloc_component(
        sdk: &'static Sdk,
        width: u32,
        height: u32,
        format: i32,
        component: i32,
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
                component,
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

    /// A lighting environment in host memory, where the relighting effect
    /// prefers it (it processes it when it is bound): RGB floats, tightly
    /// pitched, filled from `map`.
    fn host_hdri(sdk: &'static Sdk, map: &hdri::Hdri) -> Result<GpuImage, BackendError> {
        let mut image = Box::new(NvCvImage::empty());
        // SAFETY: `image` is an empty descriptor; host memory, tight rows.
        let rc = unsafe {
            (sdk.NvCVImage_Alloc)(
                &mut *image,
                map.width,
                map.height,
                ffi::NVCV_RGB,
                ffi::NVCV_F32,
                ffi::NVCV_CHUNKY,
                ffi::NVCV_CPU,
                ffi::NVCV_TIGHT_PITCH,
            )
        };
        let img = GpuImage { sdk, image };
        if rc != ffi::NVCV_SUCCESS {
            return Err(BackendError::Load {
                code: rc,
                message: format!(
                    "NvCVImage_Alloc of a {}x{} lighting environment returned {rc} ({})",
                    map.width,
                    map.height,
                    sdk.error_string(rc)
                ),
            });
        }
        let row = map.width as usize * 3;
        let pitch = img.pitch();
        if img.image.pixels.is_null() || pitch < row * 4 {
            return Err(BackendError::Load {
                code: ffi::NVCV_ERR_GENERAL,
                message: format!(
                    "NvCVImage_Alloc of a {}x{} lighting environment gave no usable buffer",
                    map.width, map.height
                ),
            });
        }
        for (y, src) in map.data.chunks_exact(row).enumerate() {
            // SAFETY: the buffer holds `height` rows of `pitch >= 4 * row`
            // bytes of host memory, suitably aligned for f32 (the SDK's
            // allocation); `y < height`.
            let dst = unsafe {
                std::slice::from_raw_parts_mut(
                    img.image.pixels.cast::<u8>().add(y * pitch).cast::<f32>(),
                    row,
                )
            };
            dst.copy_from_slice(src);
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
    /// The green screen's `Mode`, fixed at its load
    /// ([`stages::green_screen_mode`]).
    mode: u32,
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

impl Mask {
    /// Enqueues overwriting the hardened mask with the split of
    /// [`MaskFault::SplitMask`] on `stream`. Tests only.
    ///
    /// # Safety
    /// As [`Mask::harden`].
    unsafe fn split(&self, width: u32, height: u32, stream: CuStream) -> Result<(), GpuError> {
        let (h, ptr, pitch) = (height as usize, self.hard.ptr(), self.hard.pitch());
        let start = (width / 2).saturating_sub(SPLIT_MASK_RAMP / 2);
        let end = (start + SPLIT_MASK_RAMP).min(width);
        // SAFETY: every rectangle lies within the live `width x height`
        // hardened mask (`pitch >= width`) of the current context; the
        // caller guarantees the stream.
        unsafe {
            obc_gpu::memset_2d_async(ptr, pitch, 0, start as usize, h, stream)?;
            for x in start..end {
                let v = split_mask_value(x, width);
                obc_gpu::memset_2d_async(ptr + u64::from(x), pitch, v, 1, h, stream)?;
            }
            obc_gpu::memset_2d_async(
                ptr + u64::from(end),
                pitch,
                255,
                (width - end) as usize,
                h,
                stream,
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

/// The relighting effect, its lighting environment and its output. Reads
/// [`Frames::frame_bgr`] and the hardened mask; its output is valid only
/// where the mask is opaque, so only the composite reads it.
struct Relight {
    // Field order is drop order: the effect, which holds every image, goes
    // first.
    effect: Effect,
    /// The lighting environment of `light`, in host memory (where the
    /// effect prefers it). The effect processes it when it is bound and
    /// keeps its address, so it lives until the effect is destroyed or
    /// another environment is bound.
    hdri: GpuImage,
    /// The relit foreground.
    out: GpuImage,
    /// The stage's own blocking stream (one that synchronises with the
    /// legacy default stream), in the frame stream's context, bound only
    /// while a lighting environment is bound and the effect loads; every
    /// run uses the frame stream. Destroyed with the stage, after the
    /// effect and before the context is released.
    blocking: obc_gpu::Stream,
    /// The light whose environment is bound.
    light: RelightLight,
    /// The direction (degrees) and gain the effect was last given
    /// (clamped).
    pan_deg: f32,
    gain: f32,
}

impl Relight {
    /// Gives the effect the direction and the gain of `params` (clamped),
    /// setting only what changed. The call that failed and its status.
    fn apply(&mut self, params: &FrameParams) -> Result<(), (&'static str, NvCvStatus)> {
        let (pan_deg, gain) = relight_frame(params);
        if pan_deg != self.pan_deg {
            let rc = self.effect.set_f32(ffi::P_ANGLE_PAN, pan_deg.to_radians());
            if rc != ffi::NVCV_SUCCESS {
                return Err(("NvVFX_SetF32(Relighting, AnglePan)", rc));
            }
            self.pan_deg = pan_deg;
        }
        if gain != self.gain {
            let rc = self.effect.set_f32(ffi::P_FG_GAIN, gain);
            if rc != ffi::NVCV_SUCCESS {
                return Err(("NvVFX_SetF32(Relighting, FgGain)", rc));
            }
            self.gain = gain;
        }
        Ok(())
    }
}

/// The stages the last frame whose work was fully enqueued ran, so that
/// [`InferenceBackend::timings`] knows which of its events it recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stages {
    /// A per-frame background was converted since the frame before
    /// ([`MaxineBackend::set_background_frame`]).
    background: bool,
    denoise: bool,
    /// The green screen and the composite (they are loaded together).
    mask: bool,
    relight: bool,
}

/// The stage boundaries of a frame, as indices into [`Loaded::events`]:
/// before the conversion, after it, after the denoise run, after the mask,
/// after the relight run, after the effect, after the conversion back; and
/// before and after the conversion of the per-frame background that the
/// frame was the first to show, which ran before the frame (the pair is
/// recorded into [`LiveBackground::events`] and moved here by the frame).
/// A stage that is not loaded records no event, and has no step in the
/// timings.
const EV_START: usize = 0;
const EV_IN: usize = 1;
const EV_DENOISE: usize = 2;
const EV_MASK: usize = 3;
const EV_RELIGHT: usize = 4;
const EV_EFFECT: usize = 5;
const EV_OUT: usize = 6;
const EV_BG_START: usize = 7;
const EV_BG_END: usize = 8;

/// The step of [`StageTimings`] for the conversion of a per-frame
/// background ([`MaxineBackend::set_background_frame`]) into the image the
/// composite reads: in the timings of the first frame that shows it, before
/// [`StageTimings::CONVERT_IN`] (it ran before the frame). Not
/// `StageId::BACKGROUND`: a step of that name would be taken for the time
/// of the `background` stage itself.
pub const BACKGROUND_STEP: &str = "background-convert";

/// The stage times of a frame that ran `stages`, from `span(a, b)`, the
/// time between events `a` and `b`; `None` if a span is unavailable.
fn stage_timings(
    stages: Stages,
    span: impl Fn(usize, usize) -> Option<f32>,
) -> Option<StageTimings> {
    let mut t = StageTimings::new();
    if stages.background {
        t.push(BACKGROUND_STEP, span(EV_BG_START, EV_BG_END)?);
    }
    t.push(StageTimings::CONVERT_IN, span(EV_START, EV_IN)?);
    let mut last = EV_IN;
    if stages.denoise {
        t.push(StageId::DENOISE.as_str(), span(EV_IN, EV_DENOISE)?);
        last = EV_DENOISE;
    }
    if stages.mask {
        t.push(StageTimings::MASK, span(last, EV_MASK)?);
        let before_effect = if stages.relight {
            t.push(StageId::RELIGHT.as_str(), span(EV_MASK, EV_RELIGHT)?);
            EV_RELIGHT
        } else {
            EV_MASK
        };
        t.push(StageTimings::COMPOSITE, span(before_effect, EV_EFFECT)?);
        last = EV_EFFECT;
    }
    t.push(StageTimings::CONVERT_OUT, span(last, EV_OUT)?);
    Some(t)
}

/// Everything loaded for one request. Field order is drop order: the blur
/// effect uses the mask's hardened mask and the frame images, the relight,
/// denoise and green-screen effects the frame image, so the frame images go
/// last.
struct Loaded {
    width: u32,
    height: u32,
    colour: ColourSpec,
    /// The context of the load, current around every call on it.
    context: *mut c_void,
    stream: CuStream,
    effect: EffectKind,
    blur: Option<(Effect, f32)>,
    /// The load-time background image of a replace
    /// ([`LoadRequest::background`]). Never written by a per-frame
    /// background, so that `None` there returns to it.
    background: Option<GpuImage>,
    /// The per-frame backgrounds of a replace, once one was given.
    live: Option<LiveBackground>,
    /// The stages, one slot each, in reverse chain order (drop order): a
    /// post-mask stage reads the hardened mask, so its slot goes before
    /// `mask`. Each is `Some` iff the request asked for the stage (and it
    /// has not failed since).
    relight: Option<Relight>,
    denoise: Option<Denoise>,
    /// `Some` iff the effect is blur or replace, or a stage needs the mask
    /// (`needs_mask`).
    mask: Option<Mask>,
    frames: Frames,
    /// Stage boundaries of the last recorded frame (`EV_*`).
    events: [Event; 9],
    /// The stages of the last frame whose events were all recorded; `None`
    /// before the first, and while a frame is being enqueued.
    recorded: Option<Stages>,
}

impl Loaded {
    /// Whether frames run the mask: it is loaded and the effect or the
    /// relight stage reads it. A mask under `none` without the stage (kept
    /// after a failed light change, or after `reset_temporal` dropped the
    /// stage, until the next load decides) is not run.
    fn runs_mask(&self) -> bool {
        self.mask.is_some() && (self.effect != EffectKind::None || self.relight.is_some())
    }

    /// The frame images and events for `req`, with no stage yet.
    fn new(sdk: &'static Sdk, req: &LoadRequest, gpu: &GpuContext) -> Result<Loaded, BackendError> {
        Ok(Loaded {
            width: req.width,
            height: req.height,
            colour: req.colour,
            context: gpu.context,
            stream: gpu.stream,
            effect: EffectKind::None,
            blur: None,
            background: None,
            live: None,
            relight: None,
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
                Event::new()?,
                Event::new()?,
                Event::new()?,
            ],
            recorded: None,
        })
    }
}

/// The per-frame backgrounds of a replace
/// ([`MaxineBackend::set_background_frame`]): two BGR images of the frame
/// size, the SDK's GPU pitch (as the load-time image), converted into in
/// turn. A conversion writes the image the composite does not read, and
/// only a conversion that was enqueued whole makes it the one read, so a
/// failure leaves the background in force untouched.
///
/// Everything is on the load's stream, so stream order alone makes it
/// safe: a composite enqueued before a conversion runs before it (and the
/// conversion writes the other image anyway), the composite of the next
/// frame is enqueued after it and reads the finished picture, and the
/// conversion after that writes the image the frame before last read,
/// once that composite has run. No host synchronisation is needed.
struct LiveBackground {
    images: [GpuImage; 2],
    /// The image the composite reads; `None`: the load-time image.
    shown: Option<usize>,
    /// Before and after the last conversion, moved into the timings of the
    /// next frame ([`EV_BG_START`]).
    events: [Event; 2],
    /// A conversion was enqueued that no frame has shown yet.
    fresh: bool,
    /// The SDK's scratch image for these conversions, apart from the
    /// frames' ([`Frames::staging`]).
    staging: Box<NvCvImage>,
}

impl LiveBackground {
    fn alloc(sdk: &'static Sdk, width: u32, height: u32) -> Result<LiveBackground, BackendError> {
        Ok(LiveBackground {
            images: [
                GpuImage::alloc(sdk, width, height, ffi::NVCV_BGR)?,
                GpuImage::alloc(sdk, width, height, ffi::NVCV_BGR)?,
            ],
            shown: None,
            events: [Event::new()?, Event::new()?],
            fresh: false,
            staging: Box::new(NvCvImage::empty()),
        })
    }

    fn shown(&self) -> Option<&GpuImage> {
        self.shown.map(|i| &self.images[i])
    }
}

impl Drop for LiveBackground {
    fn drop(&mut self) {
        // SAFETY: the staging image is empty or was grown by the SDK's
        // transfers; it is freed once.
        unsafe { (self.images[0].sdk.NvCVImage_Dealloc)(&mut *self.staging) };
    }
}

/// Enqueues the conversion of the NV12 frame `src` into the BGR image
/// `dst` on `stream` (`NvCVImage_TransferFromYUV`, with the colour of
/// `flags`), through the SDK's scratch image `staging`.
///
/// # Safety
/// `src` describes live NV12 device memory of the current context, read
/// only (U and V interleaved in the UV plane, V one byte after U); `dst`
/// and `staging` are the backend's images of that context, and `stream` a
/// live stream of it.
unsafe fn transfer_from_nv12(
    sdk: &Sdk,
    src: &DeviceNv12,
    flags: u32,
    dst: &mut NvCvImage,
    stream: CuStream,
    staging: &mut NvCvImage,
) -> NvCvStatus {
    // SAFETY: the caller's contract.
    unsafe {
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
            dst,
            std::ptr::null(),
            1.0,
            stream,
            staging,
        )
    }
}

/// What [`MaxineBackend::configure`] loaded.
struct Configured {
    /// The wall time of the warm-up runs of the models it loaded, if any.
    warmup_ms: Option<f64>,
    /// Whether it loaded a model (the denoise, green-screen or relighting
    /// one).
    loaded_model: bool,
    /// The models it loaded and the light it rebound, one count each.
    counted: ModelLoads,
}

/// What a [`MaxineBackend`] has loaded since it was created, per model,
/// and how many live light changes its relighting stage took
/// ([`MaxineBackend::model_loads`]). Only loads that succeeded count. A
/// switch that keeps a stage loads nothing for it, so these tell which
/// stages a change of the request really loaded: a diagnostic for tests and
/// tools.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModelLoads {
    /// Denoise models loaded.
    pub denoise: u64,
    /// Green-screen models loaded (with the mask they write).
    pub green_screen: u64,
    /// Relighting models loaded.
    pub relight: u64,
    /// New lights given to a loaded relighting stage (no model load).
    pub relight_rebinds: u64,
}

impl ModelLoads {
    fn add(&mut self, o: ModelLoads) {
        self.denoise += o.denoise;
        self.green_screen += o.green_screen;
        self.relight += o.relight;
        self.relight_rebinds += o.relight_rebinds;
    }
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
    relight_faults: RelightFaultSwitch,
    denoise_strength: Result<f32, String>,
    loaded: Option<Loaded>,
    last_timings: RefCell<Option<StageTimings>>,
    model_loads: ModelLoads,
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

/// What messages call `stage`.
fn stage_name(stage: &StageId) -> String {
    match stage.as_str() {
        "denoise" => "the video noise removal".into(),
        "relight" => "the relighting".into(),
        other => format!("the {other} stage"),
    }
}

/// Turns any failure while loading `stage` into [`BackendError::Stage`], so
/// the caller can tell that the rest of its request would load. The driver
/// missing (or parked) stays `Unavailable`: nothing else would load either.
fn stage_load_err(stage: StageId, e: BackendError) -> BackendError {
    let (code, message) = match e {
        BackendError::Load { code, message }
        | BackendError::Run { code, message }
        | BackendError::Stage { code, message, .. } => (code, message),
        BackendError::InvalidInput(message) => (ffi::NVCV_ERR_GENERAL, message),
        other @ BackendError::Unavailable(_) => return other,
    };
    BackendError::Stage {
        message: format!("{} could not be loaded: {message}", stage_name(&stage)),
        stage,
        code,
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

    /// Whether the relight stage is loaded (it is dropped when a frame's
    /// relight run fails).
    pub fn relight_loaded(&self) -> bool {
        self.loaded.as_ref().is_some_and(|l| l.relight.is_some())
    }

    /// Whether the green-screen mask is loaded (for an effect, or for the
    /// relight stage under `none`).
    pub fn mask_loaded(&self) -> bool {
        self.loaded.as_ref().is_some_and(|l| l.mask.is_some())
    }

    /// The `Mode` the loaded green screen runs in
    /// ([`GREEN_SCREEN_MODE_CHAIR_FOREGROUND`] or
    /// [`GREEN_SCREEN_MODE_CHAIR_BACKGROUND`]), if one is loaded.
    pub fn mask_mode(&self) -> Option<u32> {
        self.loaded
            .as_ref()
            .and_then(|l| l.mask.as_ref())
            .map(|m| m.mode)
    }

    /// The models loaded and the light changes taken since this backend
    /// was created ([`ModelLoads`]).
    pub fn model_loads(&self) -> ModelLoads {
        self.model_loads
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

    /// Checks `req` before any GPU call. Everything but the stages first,
    /// so that [`BackendError::Stage`] means the rest would load.
    fn validate(req: &LoadRequest) -> Result<(), BackendError> {
        if req.effect == EffectKind::None && req.stages.is_empty() {
            return Err(BackendError::InvalidInput(
                "the Maxine backend has nothing to load for effect none without a stage \
                 (denoise, relight)"
                    .into(),
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
        if stages::request_needs_mask(req) && (req.width < MIN_WIDTH || req.height < MIN_HEIGHT) {
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
        for stage in req.stages.as_slice() {
            match stage.as_str() {
                "denoise" => {
                    if !((DENOISE_MIN_WIDTH..=DENOISE_MAX_WIDTH).contains(&req.width)
                        && (DENOISE_MIN_HEIGHT..=DENOISE_MAX_HEIGHT).contains(&req.height))
                    {
                        return Err(BackendError::Stage {
                            stage: stage.clone(),
                            code: ffi::NVCV_ERR_RESOLUTION,
                            message: format!(
                                "the video noise removal does not support {}x{} frames: it \
                                 takes {DENOISE_MIN_WIDTH}x{DENOISE_MIN_HEIGHT} up to \
                                 {DENOISE_MAX_WIDTH}x{DENOISE_MAX_HEIGHT}",
                                req.width, req.height
                            ),
                        });
                    }
                }
                // The green screen's limits, checked above, are the
                // stage's.
                "relight" => {}
                // Not a stage of this backend: it fails open, like a stage
                // that cannot load.
                _ => {
                    return Err(BackendError::Stage {
                        stage: stage.clone(),
                        code: ffi::NVCV_ERR_UNIMPLEMENTED,
                        message: format!(
                            "the Maxine backend has no stage {stage} (it has denoise and relight)"
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    /// Creates and loads the denoise effect from a new input image into the
    /// frame image, with its temporal state, then runs it once to absorb the
    /// first-run warm-up and zeroes the state again. Every failure is
    /// [`BackendError::Stage`] (or `Unavailable`).
    fn load_denoise(
        &self,
        frames: &mut Frames,
        (w, h): (u32, u32),
        stream: CuStream,
        strength: f32,
    ) -> Result<(Denoise, f64), BackendError> {
        self.build_denoise(frames, (w, h), stream, strength)
            .map_err(|e| stage_load_err(StageId::DENOISE, e))
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

    /// Creates and loads the green-screen effect in `mode` on the frame
    /// image, with the masks and the hardening kernel, then runs it once to
    /// absorb the first-run warm-up.
    fn load_mask(
        &self,
        frames: &mut Frames,
        (w, h): (u32, u32),
        stream: CuStream,
        mode: u32,
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
        // Before the load, which fixes it: the quality mode, with a chair as
        // foreground or as background.
        effect.set_u32(ffi::P_MODE, mode, dir)?;
        effect.set_u32(ffi::P_CUDA_GRAPH, 1, dir)?;
        effect.set_u32(ffi::P_MAX_INPUT_WIDTH, w, dir)?;
        effect.set_u32(ffi::P_MAX_INPUT_HEIGHT, h, dir)?;
        effect.set_u32(ffi::P_MAX_NUMBER_STREAMS, 1, dir)?;
        effect.set_image(ffi::P_INPUT_IMAGE_0, &mut frames.frame_bgr.image, dir)?;
        effect.set_image(ffi::P_OUTPUT_IMAGE_0, &mut mask_img.image, dir)?;
        let mut mask = Mask {
            effect,
            state: Box::new([std::ptr::null_mut()]),
            mode,
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

    /// Creates and loads the relighting effect on the frame image and the
    /// hardened mask, with the lighting environment of `light` ([`hdri`])
    /// and its output image, then runs it once to absorb the first-run
    /// warm-up. The lighting environment is bound and the effect loaded on
    /// the stage's own blocking stream ([`Relight::blocking`]); every run
    /// after that is on `stream`. Every failure is [`BackendError::Stage`]
    /// (or `Unavailable`).
    fn load_relight(
        &self,
        frames: &mut Frames,
        mask: &mut Mask,
        (w, h): (u32, u32),
        stream: CuStream,
        light: RelightLight,
    ) -> Result<(Relight, f64), BackendError> {
        self.build_relight(frames, mask, (w, h), stream, light)
            .map_err(|e| stage_load_err(StageId::RELIGHT, e))
    }

    fn build_relight(
        &self,
        frames: &mut Frames,
        mask: &mut Mask,
        (w, h): (u32, u32),
        stream: CuStream,
        light: RelightLight,
    ) -> Result<(Relight, f64), BackendError> {
        let sdk = self.sdk;
        let dir = self.model_dir.as_path();
        let mut out = GpuImage::alloc(sdk, w, h, ffi::NVCV_BGR)?;
        // Cleared once, so no earlier allocation's bytes can ever be read
        // from it (the effect writes it only where it relights).
        // SAFETY: a live allocation of `h` rows of `pitch >= 3w` bytes in
        // the current context; the caller has synchronised `stream`, and
        // the wait below finishes the fill before the effect is created.
        unsafe {
            obc_gpu::memset_2d_async(
                out.ptr(),
                out.pitch(),
                0,
                w as usize * 3,
                h as usize,
                stream,
            )
            .and_then(|()| obc_gpu::stream_synchronize(stream))
        }
        .map_err(|e| cuda_load_err(e, "clearing its output failed"))?;
        let mut hdri = GpuImage::host_hdri(sdk, &hdri::studio(light))?;
        let blocking = obc_gpu::Stream::new(false)
            .map_err(|e| cuda_load_err(e, "creating its stream failed"))?;
        let selector = match self.relight_faults.get() {
            Some(RelightFault::Load) => FAULT_SELECTOR,
            _ => ffi::FX_RELIGHTING,
        };
        let effect = Effect::create(sdk, selector, dir)?;
        effect.set_model_dir(dir)?;
        // The blocking stream from before the lighting environment is bound
        // until the load has finished: see the module documentation (step
        // 5).
        effect.set_stream(blocking.raw(), dir)?;
        effect.set_image(ffi::P_INPUT_IMAGE_0, &mut frames.frame_bgr.image, dir)?;
        // The hardened mask, never the raw one.
        effect.set_image(ffi::P_INPUT_IMAGE_1, &mut mask.hard.image, dir)?;
        effect.set_image(ffi::P_INPUT_IMAGE_2, &mut hdri.image, dir)?;
        effect.set_image(ffi::P_OUTPUT_IMAGE_0, &mut out.image, dir)?;
        effect.load(dir)?;
        blocking
            .synchronize()
            .map_err(|e| cuda_load_err(e, "its load failed on the GPU"))?;
        effect.set_stream(stream, dir)?;
        let (pan_deg, gain) = relight_frame(&FrameParams::default());
        let rc = effect.set_f32(ffi::P_FG_GAIN, gain);
        effect.check("NvVFX_SetF32 FgGain", rc, dir)?;
        let rc = effect.set_f32(ffi::P_ANGLE_PAN, pan_deg.to_radians());
        effect.check("NvVFX_SetF32 AnglePan", rc, dir)?;
        let r = Relight {
            effect,
            hdri,
            out,
            blocking,
            light,
            pan_deg,
            gain,
        };
        // Warm-up: one synchronous run on whatever the frame image and the
        // mask hold (the caller has synchronised the stream, and only the
        // stage's own output is written).
        let t1 = Instant::now();
        let rc = r.effect.run(false);
        r.effect.check("NvVFX_Run (warm-up)", rc, dir)?;
        // SAFETY: as in `sync`.
        unsafe { obc_gpu::stream_synchronize(stream) }
            .map_err(|e| cuda_load_err(e, "its warm-up run failed on the GPU"))?;
        Ok((r, ms_since(t1)))
    }

    /// Gives the loaded relighting effect `r` the lighting environment of
    /// `light`, live, without a new load: a new environment (a bound one
    /// cannot be overwritten) bound on the stage's blocking stream, which is
    /// then synchronised, and `stream` (idle: the caller synchronised it)
    /// bound again for the runs. On failure the stage's state is unknown and
    /// the caller drops it; every failure is [`BackendError::Stage`].
    fn rebind_relight(
        &self,
        r: &mut Relight,
        stream: CuStream,
        light: RelightLight,
    ) -> Result<(), BackendError> {
        let dir = self.model_dir.as_path();
        if self.relight_faults.get() == Some(RelightFault::Rebind) {
            return Err(BackendError::Stage {
                stage: StageId::RELIGHT,
                code: ffi::NVCV_ERR_GENERAL,
                message: "the relighting could not take its new light: a forced fault failed \
                          the rebind"
                    .into(),
            });
        }
        let rebind = |r: &mut Relight| -> Result<(), BackendError> {
            let mut hdri = GpuImage::host_hdri(self.sdk, &hdri::studio(light))?;
            r.effect.set_stream(r.blocking.raw(), dir)?;
            let bound = r
                .effect
                .set_image(ffi::P_INPUT_IMAGE_2, &mut hdri.image, dir);
            let synced = r
                .blocking
                .synchronize()
                .map_err(|e| cuda_load_err(e, "binding its new light failed on the GPU"));
            let restored = r.effect.set_stream(stream, dir);
            if bound.is_ok() {
                // The effect holds the new environment's address now; the
                // old one is no longer read.
                r.hdri = hdri;
                r.light = light;
            }
            bound.and(synced).and(restored)
        };
        rebind(r).map_err(|e| stage_load_err(StageId::RELIGHT, e))
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
    /// first, before anything of `l` changes, in chain order: the pre-mask
    /// stages (each fails with [`BackendError::Stage`]), then the green
    /// screen in `mask_mode` (a failure of which is the relight stage's under
    /// `none`, where only that stage needs it; a loaded one of another mode
    /// is replaced, with the relight and blur effects bound to it), then the
    /// post-mask stages, then the blur or the background, then a new light
    /// on a loaded relight stage. On
    /// failure `l` is as it was, except that a failed new light drops the
    /// relight stage ([`BackendError::Stage`] for it). Only then are the
    /// parts the request no longer needs dropped.
    fn configure(
        &self,
        l: &mut Loaded,
        req: &LoadRequest,
        strength: Option<f32>,
        light: RelightLight,
        mask_mode: u32,
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
        let needs_mask = stages::request_needs_mask(req);
        let relight = req.stages.contains(StageId::RELIGHT.as_str());
        // The green screen's mode is fixed at its load: a loaded mask of
        // another mode is not reused.
        let remask = needs_mask && l.mask.as_ref().is_none_or(|m| m.mode != mask_mode);
        let mut new_mask = if remask {
            let (m, ms) = self
                .load_mask(&mut l.frames, size, stream, mask_mode)
                .map_err(|e| match req.effect {
                    // Only the relight stage needs the mask: it fails open.
                    EffectKind::None => stage_load_err(StageId::RELIGHT, e),
                    _ => e,
                })?;
            *warmup_ms.get_or_insert(0.0) += ms;
            Some(m)
        } else {
            None
        };
        // The effects that read the mask are bound to its images: a new mask
        // in place of a loaded one needs them anew, on the new mask.
        let replaces_mask = new_mask.is_some() && l.mask.is_some();
        // A new light on a loaded stage is a live rebind, done last (below).
        let new_relight = if relight && (l.relight.is_none() || replaces_mask) {
            let mask = new_mask
                .as_mut()
                .or(l.mask.as_mut())
                .expect("relight has a mask: loaded above or kept");
            let (r, ms) = self.load_relight(&mut l.frames, mask, size, stream, light)?;
            *warmup_ms.get_or_insert(0.0) += ms;
            Some(r)
        } else {
            None
        };
        let new_blur = if req.effect == EffectKind::Blur && (l.blur.is_none() || replaces_mask) {
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
        // The last step that can fail: a new light on the loaded stage. A
        // failed rebind leaves the stage in an unknown state, so it goes,
        // and only it: the rest of `l` is the previous chain, which the
        // error (the stage's) tells the caller. The mask stays even where
        // only the stage needed it (`none`): it is not run without the
        // stage (`Loaded::runs_mask`), and whether it is kept is the next
        // commit's decision, as for every part. The parts built for this
        // request go first, the blur (which reads the hardened mask of
        // `new_mask` or of `l`) before the mask.
        let mut rebound = false;
        if relight
            && new_relight.is_none()
            && let Some(r) = l.relight.as_mut()
            && r.light != light
        {
            if let Err(e) = self.rebind_relight(r, stream, light) {
                drop(new_blur);
                drop(new_relight);
                drop(new_mask);
                drop(new_denoise);
                l.relight = None;
                return Err(e);
            }
            rebound = true;
        }
        // Commit; nothing below fails. The parts dropped here have no queued
        // work: the caller synchronised the stream.
        let loaded_model = new_denoise.is_some() || new_mask.is_some() || new_relight.is_some();
        let counted = ModelLoads {
            denoise: new_denoise.is_some().into(),
            green_screen: new_mask.is_some().into(),
            relight: new_relight.is_some().into(),
            relight_rebinds: rebound.into(),
        };
        if new_denoise.is_some() {
            l.denoise = new_denoise;
        } else if strength.is_none() {
            l.denoise = None;
        }
        // Before the mask, whose hardened mask it reads: a relight stage
        // replaced with the mask goes while the mask it reads is alive.
        if new_relight.is_some() {
            l.relight = new_relight;
        } else if !relight {
            l.relight = None;
        }
        match req.effect {
            EffectKind::Blur => {
                if new_blur.is_some() {
                    l.blur = new_blur;
                }
                l.background = None;
                l.live = None;
            }
            EffectKind::Replace => {
                l.blur = None;
                // A load shows its own background image until the next
                // per-frame one; the images stay for it.
                if let Some(v) = l.live.as_mut() {
                    v.shown = None;
                    v.fresh = false;
                }
            }
            EffectKind::None => {
                l.blur = None;
                l.background = None;
                l.live = None;
            }
        }
        // After the blur and relighting effects, which read the hardened
        // mask.
        if new_mask.is_some() {
            l.mask = new_mask;
        } else if !needs_mask {
            l.mask = None;
        }
        l.effect = req.effect;
        Ok(Configured {
            warmup_ms,
            loaded_model,
            counted,
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
        let mask_fault = self.faults.get();
        let block = match mask_fault {
            Some(MaskFault::Launch) => FAULT_BLOCK,
            _ => LUT8_BLOCK,
        };
        let split_mask = mask_fault == Some(MaskFault::SplitMask);
        let denoise_fault = self.denoise_faults.get() == Some(DenoiseFault::Run);
        let relight_fault = self.relight_faults.get() == Some(RelightFault::Run);
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
        let runs_mask = l.runs_mask();
        // The first frame after a per-frame background carries its
        // conversion's events.
        let background = match l.live.as_mut() {
            Some(v) if v.fresh => {
                v.fresh = false;
                l.events[EV_BG_START..=EV_BG_END].swap_with_slice(&mut v.events);
                true
            }
            _ => false,
        };
        let stages = Stages {
            background,
            denoise: l.denoise.is_some(),
            mask: runs_mask,
            relight: l.relight.is_some(),
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
        let rc = unsafe { transfer_from_nv12(sdk, src, flags, converted, stream, staging) };
        if rc != ffi::NVCV_SUCCESS {
            return Err(run_err("NvCVImage_TransferFromYUV", rc));
        }
        record(&l.events[EV_IN], stream)?;
        // The pre-mask stages, in canonical order; each writes the frame
        // image.
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
                return Err(BackendError::Stage {
                    stage: StageId::DENOISE,
                    code: rc,
                    message,
                });
            }
            record(&l.events[EV_DENOISE], stream)?;
        }
        // The mask, iff the effect or a stage needs it (`needs_mask`), then
        // the post-mask stages, then the effect's composite. Under `none` the
        // mask (and the composite) exist for the relight stage alone, so
        // their failures are the stage's.
        let stage_only = l.effect == EffectKind::None;
        let fail = |e: BackendError| {
            if stage_only {
                PostMaskError::Relight(e)
            } else {
                PostMaskError::Effect(e)
            }
        };
        let post: Result<PostMaskOutput, PostMaskError> = 'post: {
            let Some(m) = l.mask.as_ref().filter(|_| runs_mask) else {
                break 'post Ok(PostMaskOutput::Frame);
            };
            let rc = m.effect.run(true);
            if rc != ffi::NVCV_SUCCESS {
                break 'post Err(fail(run_err("NvVFX_Run(GreenScreen)", rc)));
            }
            // SAFETY: the masks, the table and the kernel are ours, in the
            // current context; the effect's run is queued on `stream`
            // before the kernel, which therefore reads the finished mask.
            if let Err(e) = unsafe { m.harden(l.width, l.height, stream, block) } {
                break 'post Err(fail(e.into()));
            }
            // SAFETY: as above.
            if split_mask && let Err(e) = unsafe { m.split(l.width, l.height, stream) } {
                break 'post Err(fail(e.into()));
            }
            if let Err(e) = record(&l.events[EV_MASK], stream) {
                break 'post Err(fail(e));
            }
            // The post-mask stages, in canonical order: the relit
            // foreground, valid only where the hardened mask is opaque.
            let relit: Option<&NvCvImage> = match l.relight.as_mut() {
                None => None,
                Some(r) => {
                    if let Err((call, rc)) = r.apply(params) {
                        break 'post Err(PostMaskError::Relight(run_err(call, rc)));
                    }
                    let rc = if relight_fault {
                        ffi::NVCV_ERR_GENERAL
                    } else {
                        r.effect.run(true)
                    };
                    if rc != ffi::NVCV_SUCCESS {
                        let message = if relight_fault {
                            "a forced fault failed NvVFX_Run(Relighting)".to_owned()
                        } else {
                            format!(
                                "NvVFX_Run(Relighting) returned {rc} ({})",
                                sdk.error_string(rc)
                            )
                        };
                        break 'post Err(PostMaskError::Relight(BackendError::Run {
                            code: rc,
                            message,
                        }));
                    }
                    if let Err(e) = record(&l.events[EV_RELIGHT], stream) {
                        break 'post Err(fail(e));
                    }
                    Some(&*r.out.image)
                }
            };
            // A composite through the hardened mask, never the raw one.
            let composite = |fg: &NvCvImage, bg: &NvCvImage, dst: &mut NvCvImage| {
                // SAFETY: all four images are live GPU allocations of the
                // same size in the current context; `dst` is neither input.
                unsafe { (sdk.NvCVImage_Composite)(fg, bg, &*m.hard.image, dst, stream) }
            };
            // A failed composite of the relit foreground is the stage's:
            // the frame can run again without it.
            let composite_err = |rc: NvCvStatus| match relit {
                Some(_) => PostMaskError::Relight(run_err("NvCVImage_Composite", rc)),
                None => fail(run_err("NvCVImage_Composite", rc)),
            };
            let output = match l.effect {
                EffectKind::Blur => {
                    let Some((blur, current)) = l.blur.as_mut() else {
                        break 'post Err(PostMaskError::Effect(not_loaded()));
                    };
                    let strength = params.blur_strength.clamp(0.0, 1.0);
                    if strength != *current {
                        let rc = blur.set_f32(ffi::P_STRENGTH, strength);
                        if rc != ffi::NVCV_SUCCESS {
                            break 'post Err(PostMaskError::Effect(run_err(
                                "NvVFX_SetF32(Strength)",
                                rc,
                            )));
                        }
                        *current = strength;
                    }
                    let rc = blur.run(true);
                    if rc != ffi::NVCV_SUCCESS {
                        break 'post Err(PostMaskError::Effect(run_err(
                            "NvVFX_Run(BackgroundBlur)",
                            rc,
                        )));
                    }
                    match relit {
                        // The relit foreground over the blurred frame, into
                        // the frame image: the relighting and the blur have
                        // read it already (stream order).
                        Some(fg) => {
                            let rc = composite(fg, &out_bgr.image, &mut frame_bgr.image);
                            if rc != ffi::NVCV_SUCCESS {
                                break 'post Err(composite_err(rc));
                            }
                            PostMaskOutput::Frame
                        }
                        None => PostMaskOutput::Out,
                    }
                }
                EffectKind::Replace => {
                    // The per-frame background in force, else the
                    // load-time one.
                    let shown = l.live.as_ref().and_then(LiveBackground::shown);
                    let Some(bg) = shown.or(l.background.as_ref()) else {
                        break 'post Err(PostMaskError::Effect(not_loaded()));
                    };
                    let fg = relit.unwrap_or(&frame_bgr.image);
                    let rc = composite(fg, &bg.image, &mut out_bgr.image);
                    if rc != ffi::NVCV_SUCCESS {
                        break 'post Err(composite_err(rc));
                    }
                    PostMaskOutput::Out
                }
                // A mask under `none` exists only for the relight stage:
                // its foreground over the (denoised) camera frame.
                EffectKind::None => {
                    let Some(fg) = relit else {
                        break 'post Err(PostMaskError::Effect(not_loaded()));
                    };
                    let rc = composite(fg, &frame_bgr.image, &mut out_bgr.image);
                    if rc != ffi::NVCV_SUCCESS {
                        break 'post Err(composite_err(rc));
                    }
                    PostMaskOutput::Out
                }
            };
            if let Err(e) = record(&l.events[EV_EFFECT], stream) {
                break 'post Err(fail(e));
            }
            Ok(output)
        };
        let out: &NvCvImage = match post {
            Ok(PostMaskOutput::Frame) => &frame_bgr.image,
            Ok(PostMaskOutput::Out) => &out_bgr.image,
            Err(PostMaskError::Effect(e)) => return Err(e),
            Err(PostMaskError::Relight(e)) => {
                // What was queued must finish before the stage (and, under
                // `none`, the mask it alone needed) goes.
                let synced = sync(stream);
                l.relight = None;
                if l.effect == EffectKind::None {
                    l.mask = None;
                }
                let (code, what) = match e {
                    BackendError::Load { code, message }
                    | BackendError::Run { code, message }
                    | BackendError::Stage { code, message, .. } => (code, message),
                    BackendError::InvalidInput(m) | BackendError::Unavailable(m) => {
                        (ffi::NVCV_ERR_GENERAL, m)
                    }
                };
                let mut message = format!(
                    "the relighting failed and was turned off; the next frame runs without it: \
                     {what}"
                );
                if let Err(e) = synced {
                    message.push_str(&format!(
                        " (waiting for the stream before that failed: {e})"
                    ));
                }
                return Err(BackendError::Stage {
                    stage: StageId::RELIGHT,
                    code,
                    message,
                });
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

/// Which image a frame's post-mask part left the output in.
enum PostMaskOutput {
    /// [`Frames::frame_bgr`]: no mask, or the relit foreground composited
    /// over the blur.
    Frame,
    /// [`Frames::out_bgr`].
    Out,
}

/// Whose failure a frame's post-mask part ran into.
enum PostMaskError {
    /// The effect's: the frame fails.
    Effect(BackendError),
    /// The relight stage's (or, under `none`, the mask's, which only the
    /// stage needs): the stage is dropped and fails open.
    Relight(BackendError),
}

fn not_loaded() -> BackendError {
    BackendError::InvalidInput("the Maxine backend has no effect loaded".into())
}

fn no_mask() -> BackendError {
    BackendError::InvalidInput(
        "the Maxine backend has no mask loaded (effect none without the relight stage runs no \
         mask)"
            .into(),
    )
}

impl InferenceBackend for MaxineBackend {
    fn name(&self) -> &'static str {
        "maxine"
    }

    fn load(&mut self, gpu: &GpuContext, req: &LoadRequest) -> Result<LoadReport, BackendError> {
        Self::check_context(gpu)?;
        Self::validate(req)?;
        let strength = if req.stages.contains(StageId::DENOISE.as_str()) {
            Some(self.denoise_strength.clone().map_err(strength_err)?)
        } else {
            None
        };
        let light = if req.stages.contains(StageId::RELIGHT.as_str()) {
            stages::relight_light(req)?
        } else {
            RelightLight::default()
        };
        let mask_mode = stages::green_screen_mode(req)?;
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
            Loaded::new(self.sdk, req, gpu)?
        };
        match self.configure(&mut loaded, req, strength, light, mask_mode) {
            Ok(c) => {
                self.loaded = Some(loaded);
                self.model_loads.add(c.counted);
                Ok(LoadReport {
                    load_ms: ms_since(t0) - c.warmup_ms.unwrap_or(0.0),
                    warmup_ms: c.warmup_ms,
                    reused_model: reuse && !c.loaded_model,
                    notice: None,
                })
            }
            Err(e) => {
                if reuse {
                    // The previous chain stays: `configure` changed nothing,
                    // or, when a new light failed, dropped the relight stage
                    // alone (the error is the stage's).
                    self.loaded = Some(loaded);
                } else {
                    drop(loaded);
                    *self.last_timings.borrow_mut() = None;
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
        *self.last_timings.borrow_mut() = None;
    }

    fn timings(&self) -> Option<StageTimings> {
        let l = self.loaded.as_ref()?;
        let stages = l.recorded?;
        if l.events[EV_OUT].is_complete().unwrap_or(false) {
            let e = &l.events;
            let span = |a: usize, b: usize| Event::elapsed_ms(&e[a], &e[b]).ok();
            if let Some(t) = stage_timings(stages, span) {
                *self.last_timings.borrow_mut() = Some(t);
            }
        }
        self.last_timings.borrow().clone()
    }

    /// Yes: a replace takes per-frame backgrounds.
    fn supports_live_background(&self) -> bool {
        true
    }

    /// Converts `frame` (CUDA NV12 of the loaded size and colour) on the
    /// load's stream into the background image the composite reads, with
    /// the conversion of the camera frames (`NvCVImage_TransferFromYUV`),
    /// GPU to GPU. It goes into the one of two images that the composite
    /// does not read, which becomes the one read only once the conversion
    /// is enqueued, so a failure leaves the previous background; stream
    /// order does the rest ([`LiveBackground`]). The frame's memory is read
    /// by that queued conversion, in the stream order in which the caller
    /// handed it over, as the source of a frame is. `None` returns to the
    /// load-time image, which is never written. The conversion's GPU time
    /// is the [`BACKGROUND_STEP`] of the next frame's timings.
    fn set_background_frame(
        &mut self,
        frame: Option<BackgroundFrame<'_>>,
    ) -> Result<(), BackendError> {
        let sdk = self.sdk;
        let l = self.loaded.as_mut().ok_or_else(not_loaded)?;
        if l.effect != EffectKind::Replace {
            return Err(BackendError::InvalidInput(format!(
                "a per-frame background needs replace loaded, not {}",
                l.effect
            )));
        }
        let Some(frame) = frame else {
            if let Some(v) = l.live.as_mut() {
                v.shown = None;
                v.fresh = false;
            }
            return Ok(());
        };
        let src = match frame {
            BackgroundFrame::Cuda(f) => f,
            BackgroundFrame::Host(_) => {
                return Err(BackendError::InvalidInput(
                    "the Maxine backend takes CUDA background frames only".into(),
                ));
            }
        };
        src.validate()?;
        if (src.width, src.height) != (l.width, l.height) {
            return Err(BackendError::InvalidInput(format!(
                "the background frame is {}x{}, the backend is loaded for {}x{}",
                src.width, src.height, l.width, l.height
            )));
        }
        Self::check_context(&GpuContext {
            context: l.context,
            stream: l.stream,
        })?;
        if l.live.is_none() {
            l.live = Some(LiveBackground::alloc(sdk, l.width, l.height)?);
        }
        let (stream, flags) = (l.stream, colour_flags(l.colour));
        let v = l.live.as_mut().expect("allocated above");
        let target = v.shown.map_or(0, |i| 1 - i);
        record(&v.events[0], stream)?;
        let LiveBackground {
            images, staging, ..
        } = v;
        // SAFETY: `src` describes live NV12 device memory of the current
        // context (checked above), ready on `stream` (caller's contract),
        // read only; the destination and staging images are ours, and no
        // queued composite reads the destination (it is not the one shown).
        let rc = unsafe {
            transfer_from_nv12(sdk, &src, flags, &mut images[target].image, stream, staging)
        };
        if rc != ffi::NVCV_SUCCESS {
            return Err(BackendError::Run {
                code: rc,
                message: format!(
                    "NvCVImage_TransferFromYUV of the background frame returned {rc} ({})",
                    sdk.error_string(rc)
                ),
            });
        }
        record(&v.events[1], stream)?;
        v.shown = Some(target);
        v.fresh = true;
        Ok(())
    }

    /// Zeroes the denoise state (on the stream) and resets the green
    /// screen's (after waiting for the stream: the SDK does not say which
    /// stream `NvVFX_ResetState` works on, and queued frames use the state).
    ///
    /// The relighting effect's temporal state cannot be reset: it lists no
    /// `State` parameter, and handing it a state allocated with
    /// `NvVFX_AllocateState` (or resetting one) makes every later run fail
    /// with "an image buffer has not been specified". Its model blends the
    /// previous frames' albedo and normals into each new one, so after a
    /// switch of source the first frames of camera B would carry camera
    /// A's person, background included where blur or replace had hidden
    /// it. So the stage is dropped here (after the stream has finished
    /// with it) and returned: frames run without it from now, and the
    /// caller loads it again, which builds a new effect with a fresh state
    /// (the full sequence of `build_relight`, the blocking stream included)
    /// before any frame is relit again. The mask stays (not run under
    /// `none` without the stage: `Loaded::runs_mask`), so the new stage
    /// costs its own load only.
    fn reset_temporal(&mut self, gpu: &GpuContext) -> Result<StageList, BackendError> {
        Self::check_context(gpu)?;
        let Some(l) = self.loaded.as_mut() else {
            return Ok(StageList::default());
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
        if l.relight.is_some() {
            // Queued frames may still run it.
            sync(l.stream)?;
            l.relight = None;
            return Ok(StageId::RELIGHT.into());
        }
        Ok(StageList::default())
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
    use obc_backend::StageList;
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
        assert!(m.contains("[effect] maxine_root"), "{m}");
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
        assert!(m.contains("no [effect] maxine_root is configured"), "{m}");
        assert!(m.contains(DEFAULT_ROOT), "{m}");
        assert!(m.contains("maxine-vfx-sdk-local"), "{m}");
    }

    #[test]
    fn a_library_that_is_present_but_unloadable_names_the_sdk_directories() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("lib");
        std::fs::create_dir(&lib).unwrap();
        // Present under the expected name, but not an ELF object.
        let bad = lib.join("libobc-no-such-NVCVImage.so.1");
        std::fs::write(&bad, b"not a library").unwrap();
        let e = failing(Some(dir.path().to_path_buf())).probe().unwrap_err();
        assert!(matches!(e, BackendError::Unavailable(_)), "{e:?}");
        let m = e.to_string();
        assert!(m.contains("could not be loaded"), "{m}");
        assert!(m.contains("must be a complete SDK"), "{m}");
        assert!(m.contains("external/cuda/lib"), "{m}");
        assert!(!m.contains("LD_LIBRARY_PATH"), "no longer needed: {m}");
        // The library was tried first by absolute path, and that failure
        // is part of the reason.
        assert!(
            m.contains("SDK libraries that could not be loaded first"),
            "{m}"
        );
        let canonical = bad.canonicalize().unwrap();
        assert!(m.contains(&format!("{}: ", canonical.display())), "{m}");
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
            Some(MaskFault::SplitMask),
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
            stages: StageList::default(),
            width,
            height,
            colour: ColourSpec::LIMITED_BT601,
            background,
            params: Default::default(),
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
            stages: StageId::DENOISE.into(),
            width,
            height,
            colour: ColourSpec::LIMITED_BT601,
            background: None,
            params: Default::default(),
        }
    }

    #[test]
    fn none_needs_the_denoise_stage_and_skips_the_green_screen_minimum() {
        let off = LoadRequest {
            stages: StageList::default(),
            ..denoise_req(EffectKind::None, 1920, 1080)
        };
        let e = MaxineBackend::validate(&off).unwrap_err();
        assert!(matches!(e, BackendError::InvalidInput(_)), "{e:?}");
        assert!(e.to_string().contains("without a stage"), "{e}");
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
            let BackendError::Stage {
                stage,
                code,
                message,
            } = &e
            else {
                panic!("{w}x{h}: expected a denoise stage error, got {e:?}");
            };
            assert_eq!(*stage, StageId::DENOISE);
            assert_eq!(*code, ffi::NVCV_ERR_RESOLUTION);
            assert!(message.contains(&format!("{w}x{h}")), "{message}");
            assert!(message.contains("80x80 up to 1920x1080"), "{message}");
        }
        // The same size with an effect: the effect part is fine, so the
        // error is the denoise stage's, and without denoise it loads.
        let e = MaxineBackend::validate(&denoise_req(EffectKind::Blur, 2048, 1152)).unwrap_err();
        assert!(
            matches!(
                e,
                BackendError::Stage { ref stage, .. } if *stage == StageId::DENOISE
            ),
            "{e:?}"
        );
        let off = LoadRequest {
            stages: StageList::default(),
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
    fn the_root_is_the_variable_else_the_configured_root_else_the_default_else_none() {
        use std::ffi::OsStr;
        let default = tempfile::tempdir().unwrap();
        let present = default.path();
        let absent = present.join("no-sdk");
        let configured = Path::new("/configured/sdk");
        // Set and non-empty: that root, whatever is configured and whether
        // or not the default exists.
        for d in [present, absent.as_path()] {
            for c in [None, Some(configured)] {
                assert_eq!(
                    resolve_root(Some(OsStr::new("/some/sdk")), c, d),
                    Some(PathBuf::from("/some/sdk"))
                );
            }
        }
        for v in [None, Some(OsStr::new(""))] {
            // Unset or empty: the configured root, existing or not, over
            // the default.
            for d in [present, absent.as_path()] {
                assert_eq!(
                    resolve_root(v, Some(configured), d),
                    Some(configured.into())
                );
            }
            // Nothing configured: the default when it is a directory.
            assert_eq!(resolve_root(v, None, present), Some(present.to_path_buf()));
            assert_eq!(resolve_root(v, None, &absent), None);
        }
        // A file at the default path is not an SDK root.
        let file = present.join("file");
        std::fs::write(&file, b"").unwrap();
        assert_eq!(resolve_root(None, None, &file), None);
        assert_eq!(DEFAULT_ROOT, "/opt/nvidia/maxine/videofx");
    }

    #[test]
    fn the_configured_root_is_what_from_env_uses_without_the_variable() {
        let env_root = std::env::var_os(ROOT_ENV).filter(|v| !v.is_empty());
        set_configured_root(Some("/configured/for/test".into()));
        assert_eq!(
            configured_root(),
            Some(PathBuf::from("/configured/for/test"))
        );
        let (effective, factory) = (effective_root(), MaxineFactory::from_env());
        set_configured_root(None);
        assert_eq!(configured_root(), None);
        match env_root {
            // The variable wins.
            Some(v) => {
                assert_eq!(effective, Some(PathBuf::from(&v)));
                assert_eq!(factory.root(), Some(Path::new(&v)));
            }
            None => {
                assert_eq!(effective, Some(PathBuf::from("/configured/for/test")));
                assert_eq!(factory.root(), Some(Path::new("/configured/for/test")));
            }
        }
        // Nothing is loaded in this test process, so no root conflicts.
        assert_eq!(root_conflict(Some(Path::new("/configured/for/test"))), None);
    }

    #[test]
    fn the_usable_root_is_the_loaded_one_once_the_sdk_is_loaded() {
        let configured = || Some(PathBuf::from("/configured/new"));
        // Loaded from a root: that one, whatever is configured now.
        assert_eq!(
            pick_usable_root(Some("/loaded/old".into()), configured),
            Some(PathBuf::from("/loaded/old"))
        );
        // Nothing loaded: the root in force.
        assert_eq!(
            pick_usable_root(None, configured),
            Some(PathBuf::from("/configured/new"))
        );
        assert_eq!(pick_usable_root(None, || None), None);
        // Nothing is loaded in this test process.
        assert_eq!(loaded_root(), None);
        assert_eq!(usable_root(), effective_root());
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
        assert!(
            matches!(
                e,
                BackendError::Stage { ref stage, .. } if *stage == StageId::DENOISE
            ),
            "{e:?}"
        );
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
    fn stage_load_failures_become_stage_errors_except_a_missing_driver() {
        let e = stage_load_err(
            StageId::DENOISE,
            BackendError::Load {
                code: -10,
                message: "NvVFX_Load(Denoising) returned -10".into(),
            },
        );
        match e {
            BackendError::Stage {
                stage,
                code,
                message,
            } => {
                assert_eq!(stage, StageId::DENOISE);
                assert_eq!(code, -10);
                assert!(message.contains("video noise removal"), "{message}");
                assert!(message.contains("NvVFX_Load(Denoising)"), "{message}");
            }
            other => panic!("expected Denoise, got {other:?}"),
        }
        assert!(matches!(
            stage_load_err(StageId::DENOISE, BackendError::InvalidInput("x".into())),
            BackendError::Stage { stage, .. } if stage == StageId::DENOISE
        ));
        assert!(matches!(
            stage_load_err(
                StageId::DENOISE,
                BackendError::Run {
                    code: 2,
                    message: "oom".into()
                }
            ),
            BackendError::Stage { stage, code: 2, .. } if stage == StageId::DENOISE
        ));
        assert!(matches!(
            stage_load_err(StageId::DENOISE, BackendError::Unavailable("parked".into())),
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
    fn stages_that_did_not_run_have_no_step() {
        // A fake span: one unit per event index it covers, so each figure shows
        // which events it was measured between.
        let span = |a: usize, b: usize| Some((b - a) as f32);
        let st = |denoise, mask, relight| Stages {
            background: false,
            denoise,
            mask,
            relight,
        };
        let all = stage_timings(st(true, true, true), span).unwrap();
        assert_eq!(
            all,
            StageTimings::new()
                .with(StageTimings::CONVERT_IN, 1.0)
                .with("denoise", 1.0)
                .with(StageTimings::MASK, 1.0)
                .with("relight", 1.0)
                .with(StageTimings::COMPOSITE, 1.0)
                .with(StageTimings::CONVERT_OUT, 1.0)
        );
        let no_relight = stage_timings(st(true, true, false), span).unwrap();
        assert_eq!(no_relight.step_ms("relight"), None);
        assert_eq!(
            no_relight.step_ms(StageTimings::COMPOSITE),
            Some(2.0),
            "the effect follows the mask"
        );
        assert_eq!(no_relight.total_ms(), 6.0);
        let effect_only = stage_timings(st(false, true, false), span).unwrap();
        assert_eq!(effect_only.step_ms("denoise"), None);
        assert_eq!(
            effect_only.step_ms(StageTimings::MASK),
            Some(2.0),
            "the mask follows the conversion"
        );
        assert_eq!(effect_only.total_ms(), 6.0);
        let relight_only = stage_timings(st(false, true, true), span).unwrap();
        assert_eq!(
            (
                relight_only.step_ms(StageTimings::MASK),
                relight_only.step_ms("relight")
            ),
            (Some(2.0), Some(1.0))
        );
        assert_eq!(relight_only.total_ms(), 6.0);
        let denoise_only = stage_timings(st(true, false, false), span).unwrap();
        assert_eq!(
            (
                denoise_only.step_ms(StageTimings::MASK),
                denoise_only.step_ms("relight"),
                denoise_only.step_ms(StageTimings::COMPOSITE)
            ),
            (None, None, None)
        );
        assert_eq!(
            denoise_only.step_ms(StageTimings::CONVERT_OUT),
            Some(4.0),
            "out follows the denoise"
        );
        assert_eq!(denoise_only.total_ms(), 6.0);
        let none = stage_timings(st(false, false, false), span).unwrap();
        assert_eq!(none.step_ms(StageTimings::CONVERT_OUT), Some(5.0));
        // A span that cannot be read gives no timings at all.
        assert_eq!(
            stage_timings(st(true, true, true), |a, b| (a != EV_IN)
                .then_some((b - a) as f32)),
            None
        );
        assert_eq!(
            stage_timings(st(false, true, true), |a, b| (b != EV_RELIGHT)
                .then_some((b - a) as f32)),
            None
        );
    }

    #[test]
    fn a_fresh_background_frame_is_a_step_before_the_conversion() {
        let span = |a: usize, b: usize| Some((b - a) as f32);
        let st = Stages {
            background: true,
            denoise: false,
            mask: true,
            relight: false,
        };
        let t = stage_timings(st, span).unwrap();
        assert_eq!(
            t.steps().first(),
            Some(&(BACKGROUND_STEP.to_string(), 1.0)),
            "measured between its own two events, first: it ran before the frame"
        );
        assert_eq!(t.step_ms(StageTimings::CONVERT_IN), Some(1.0));
        assert_eq!(t.total_ms(), 7.0);
        // Its span unreadable: no timings, as for any step.
        assert_eq!(
            stage_timings(st, |a, b| (a != EV_BG_START).then_some((b - a) as f32)),
            None
        );
        // Without one, no step.
        let none = stage_timings(
            Stages {
                background: false,
                ..st
            },
            span,
        )
        .unwrap();
        assert_eq!(none.step_ms(BACKGROUND_STEP), None);
        assert_ne!(BACKGROUND_STEP, StageId::BACKGROUND.as_str());
    }

    #[test]
    fn a_stage_of_another_backend_fails_alone_and_any_order_is_accepted() {
        let req = |stages: &[&str]| LoadRequest {
            stages: StageList::new(stages.iter().map(|s| StageId::new(*s).unwrap())).unwrap(),
            ..denoise_req(EffectKind::Blur, 1280, 720)
        };
        let e = MaxineBackend::validate(&req(&["denoise", "hello"])).unwrap_err();
        match &e {
            BackendError::Stage { stage, message, .. } => {
                assert_eq!(stage, "hello");
                assert!(message.contains("no stage hello"), "{message}");
            }
            other => panic!("expected a stage error, got {other:?}"),
        }
        // The pre-mask stage runs before the mask whatever the order.
        assert!(MaxineBackend::validate(&req(&["relight", "denoise"])).is_ok());
        assert!(MaxineBackend::validate(&req(&["denoise", "relight"])).is_ok());
        assert_eq!(
            stage_name(&StageId::new("hello").unwrap()),
            "the hello stage"
        );
    }

    #[test]
    fn relight_is_named_in_messages() {
        assert_eq!(stage_name(&StageId::RELIGHT), "the relighting");
        assert_eq!(
            feature_library("NvVFX_CreateEffect(Relighting)"),
            "libnvVFXRelighting.so"
        );
    }

    #[test]
    fn relight_faults_are_off_unless_armed_and_shared_by_clones() {
        let f = MaxineFactory::with_root(None);
        assert_eq!(f.relight_faults.get(), None);
        assert_eq!(MaxineFactory::from_env().relight_faults.get(), None);
        let switch = RelightFaultSwitch::default();
        let f = f.with_relight_faults(switch.clone());
        let copy = f.clone();
        for fault in [
            Some(RelightFault::Load),
            Some(RelightFault::Run),
            Some(RelightFault::Rebind),
            None,
        ] {
            switch.set(fault);
            assert_eq!(f.relight_faults.get(), fault);
            assert_eq!(copy.relight_faults.get(), fault);
        }
    }

    #[test]
    fn the_split_mask_is_background_then_a_soft_ramp_then_foreground() {
        for width in [1920u32, 640, 101] {
            let start = width / 2 - SPLIT_MASK_RAMP / 2;
            let v: Vec<u8> = (0..width).map(|x| split_mask_value(x, width)).collect();
            assert!(v[..start as usize].iter().all(|&m| m == 0), "{width}");
            let ramp = &v[start as usize..(start + SPLIT_MASK_RAMP) as usize];
            assert!(ramp.iter().all(|&m| m > 0 && m < 255), "{width}: {ramp:?}");
            assert!(ramp.windows(2).all(|p| p[0] < p[1]), "{width}: rising");
            assert!(
                v[(start + SPLIT_MASK_RAMP) as usize..]
                    .iter()
                    .all(|&m| m == 255),
                "{width}"
            );
        }
    }

    #[test]
    fn relight_stage_availability_needs_the_green_screen_too() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let lib = |d: &str, l: &str| {
            let p = root.join("features").join(d).join("lib");
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join(l), b"").unwrap();
        };
        let models = root.join("lib").join("models");
        std::fs::create_dir_all(&models).unwrap();
        lib("nvvfxrelighting", "libnvVFXRelighting.so");
        for m in ["relight_0thframe_perf_120", "relight_nthframe_perf_120"] {
            std::fs::write(models.join(format!("{m}.engine.trtpkg")), b"").unwrap();
        }
        let a = stage_availability(root, Some((12, 0)), "relight");
        let why = a.reason().expect("no green screen").to_owned();
        assert!(why.starts_with("green-screen: "), "{why}");
        lib("nvvfxgreenscreen", "libnvVFXGreenScreen.so");
        std::fs::write(models.join("AIGS_1080_120_m0.engine.trtpkg"), b"").unwrap();
        assert_eq!(
            stage_availability(root, Some((12, 0)), "relight"),
            Availability::Installed
        );
        assert!(
            !stage_availability(root, Some((12, 0)), "denoise").is_installed(),
            "the denoising feature is not installed"
        );
    }
}
