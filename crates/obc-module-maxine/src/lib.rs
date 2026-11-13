//! NVIDIA Maxine video effects as an omarchy-broadcast module.
//!
//! The library this crate builds (`libobc_module_maxine.so`, with the
//! manifest `maxine-vfx.toml`) carries the Maxine backend of
//! `obc-backend-maxine` behind the module ABI, so the daemon needs no Maxine
//! code of its own. The SDK and the CUDA driver are still opened at run
//! time, when a chain is loaded: the library loads on a machine with
//! neither.
//!
//! # Stages
//!
//! | Stage | Slot | Runs |
//! |---|---|---|
//! | `enhance` | pre-matte (position 0) | AI video super resolution or a fast upscale, alone in its chain (the ingest's) |
//! | `denoise` | pre-matte | noise removal |
//! | `background` | matte | the cut-out, and the blur or the replacement of the background |
//! | `relight` | post-matte, needs the matte | relighting of the person |
//! | `hdr` | post-composite | the HDR look: NVIDIA TrueHDR, tone-mapped back to SDR |
//! | `frame-generation` | post-composite, last | a frame between two camera frames, for an output at twice the camera rate |
//!
//! A chain of `background`, `denoise` and `relight` is the Maxine effect
//! backend ([`obc_backend_maxine::MaxineBackend`]): the `background` stage's
//! load-time parameter `mode` (`blur` or `replace`) is the effect, its
//! load-time parameter `chair` (a boolean, default `false`) whether a chair
//! is cut out with the background, its live parameter `blur_strength` (from
//! 0 to 1) the blur strength, and the chain's background frame the
//! replacement image; without `background` the effect is `none`. Under
//! replace the module takes a per-frame background (ABI 1.4,
//! `Instance::set_background`, for a video background): each CUDA NV12
//! frame is converted on the GPU into the backend's background image. A load
//! on a loaded chain is a switch on the same backend, so models are reused
//! as before; a new `chair` loads the green screen again, which the backend
//! does as any switch, the previous chain kept running until the new one is
//! in force. A chain of `enhance` alone is the Maxine upscaler
//! ([`obc_backend_maxine::vsr::MaxineVsr`]): NV12 or I420 in, NV12 an
//! integer factor (2 to 4) larger out, by the method of its load-time
//! parameter `method`: `ai` (video super resolution) or `fast` (the SDK's
//! lightweight upscale); without one, `ai` when it is installed, else
//! `fast`. Frames are CUDA frames only.
//!
//! The `frame-generation` stage (the `VideoFrameGeneration` effect,
//! [`obc_backend_maxine::vfg`]) runs last in a chain of any of the effect
//! stages, or alone, on NV12 frames of one size. The daemon adds it when the
//! output runs at twice the camera rate, so that the output clock's repeats
//! become new pictures: a fresh frame runs the other stages and goes out as
//! the frame midway between the previous result and this one, at medium
//! quality and with automatic shot-change detection; a repeat
//! (`Frame::is_repeat`, ABI 1.1) skips the other stages and goes out as the
//! last result; the first frame after a load or a temporal reset goes out
//! as it is ([`framegen`]). The output is therefore one output tick late.
//! The stage has no setting. When it fails to load, or on a frame, it alone
//! is dropped (a stage failure) and the rest of the chain runs on every
//! frame, repeats included, as without it.
//!
//! The `hdr` stage (the `TrueHDR` effect, [`obc_backend_maxine::truehdr`])
//! runs after the other stages of its chain (any of the effect stages, or
//! alone), before frame generation, on NV12 frames of one size: the frame
//! goes to RGBA, through TrueHDR into a 10-bit image (PQ, BT.2020), and
//! back to 8 bits through a tone map that keeps the mid-tones at their SDR
//! level ([`obc_gpu::tonemap`]), so an SDR output shows what the HDR pass
//! changed. The effect runs at its default parameters. It runs on every frame it gets, repeats included, holding
//! nothing: a repeat is the camera frame again, and the other stages of its
//! chain made it afresh (with, say, a new cut-out), so the look is made
//! afresh too; the effect element caches the repeats it sends itself.
//! Under frame generation a repeat runs no stage, the HDR look included
//! ([`framegen`]). Its one setting is its switch ([`HDR_LOOK`]). It is not part of the
//! cut-out: when it fails to load, or on a frame, it alone is dropped (a
//! stage failure) and the rest of the chain runs as without it.
//!
//! # Configuration
//!
//! The module's configuration table (`[module."br.com.dias.maxine-vfx"]`):
//!
//! * `sdk_root`: the SDK root. `MAXINE_VFX_ROOT` takes precedence when set;
//!   with neither, the packaged SDK's directory when it exists, else the
//!   library search path ([`obc_backend_maxine::effective_root`]). The SDK
//!   is loaded once per process, from the first root used.
//! * `_host.compute_capability`: `[major, minor]` of the GPU, reserved for
//!   the daemon, so the probe looks for the models of that architecture;
//!   absent or null, models of any architecture count.
//!
//! # Probe
//!
//! The probe reads the SDK tree and nothing else (no `dlopen`, no CUDA), so
//! it never wakes a suspended GPU. A stage is available when every SDK
//! feature it is made of is installed, with its models for the GPU; else
//! it is unavailable, [`REASON_NOT_INSTALLED`] (the detail goes to the
//! log); `enhance` is available when either of its methods is installed,
//! and a load of a method that is not fails with the reason. Relighting
//! needs the cut-out of the `background` stage, so it is
//! unavailable whenever `background` is. Frame generation needs the video
//! frame generation feature, the HDR look the TrueHDR feature. Without an SDK root to read, nothing can be
//! known, and every stage is reported available.
//!
//! The deeper check (the ABI's `check`, run before a stream starts) does
//! what the factories of the built-in backend did: it opens the SDK
//! libraries (once per process) and finds the model directory, never
//! creating a CUDA context, so a missing SDK is told before the camera
//! opens rather than when the chain loads.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use obc_backend::enhance::{
    ChromaSiting as EnhanceSiting, DeviceYuv420, EnhanceRequest, Enhancer, EnhancerFactory,
    YuvLayout,
};
use obc_backend::{
    BackendError, BackendFactory, BackgroundFrame, ColourMatrix as BackendMatrix, ColourSpec,
    DeviceNv12, EffectKind, FrameParams, GpuContext, HostBgr, InferenceBackend, LoadRequest,
    Params, StageId, StageList, StageTimings, Value as BackendValue,
};
use obc_backend_maxine::truehdr::{MaxineTrueHdr, MaxineTrueHdrFactory};
use obc_backend_maxine::vfg::{MaxineVfg, MaxineVfgFactory};
use obc_backend_maxine::vsr::{ENHANCE_METHOD, EnhanceMethod, MaxineVsr, MaxineVsrFactory};
use obc_backend_maxine::{
    BACKGROUND_CHAIR, Feature, MaxineBackend, MaxineFactory, RELIGHT_BRIGHTNESS, RELIGHT_DIRECTION,
    RELIGHT_LIGHT, RelightLight, STAGES, feature_availability, stage_availability_of,
};
use obc_module_sdk::{
    Apply, ChainRequest, ChromaSiting, ColourMatrix, Error, Frame, FrameKind, Group, Instance,
    LoadReport, Module, Op, Param, PixelFormat, ProbeReport, Role, Setting, SettingsDescription,
    Value, log, serde_json,
};
use serde::Deserialize;

pub mod framegen;
#[cfg(test)]
mod gpu_tests;

use framegen::{FrameGen, Step};

/// The module id.
pub const MODULE_ID: &str = "br.com.dias.maxine-vfx";

/// The stage id of frame generation.
pub const FRAME_GENERATION: &str = "frame-generation";
/// The stage id of the HDR look.
pub const HDR: &str = "hdr";

/// The reason of a stage whose SDK features are not installed.
pub const REASON_NOT_INSTALLED: &str = "Not installed";
/// Why a relighting setting is greyed while relighting is off.
pub const REASON_TURN_ON_RELIGHT: &str = "Turn on relighting to change this";
/// Why the enhancement method is greyed while enhancement is off.
pub const REASON_TURN_ON_ENHANCE: &str = "Turn on enhancement to change this";
/// Why the chair setting is greyed while the background effect is off.
pub const REASON_TURN_ON_BACKGROUND: &str = "Turn on the background effect to change this";

/// The `background` stage's load-time parameter with the effect.
pub const BACKGROUND_MODE: &str = "mode";
/// The `background` stage's live parameter with the blur strength.
pub const BACKGROUND_BLUR_STRENGTH: &str = "blur_strength";
/// The daemon's background effect setting, which the chair setting's
/// condition reads.
pub const DAEMON_BACKGROUND_MODE: &str = "effect.background.mode";

/// The cut-out group's setting: a chair is cut out with the background
/// (the `background` stage's [`BACKGROUND_CHAIR`]).
pub const CHAIR: &str = "effect.cutout.chair";

/// The video group's settings.
pub const DENOISE: &str = "effect.video.denoise";
/// See [`DENOISE`].
pub const ENHANCE: &str = "effect.video.enhance";
/// See [`DENOISE`]: the enhance stage's method ([`ENHANCE_METHOD`]).
pub const ENHANCE_METHOD_SETTING: &str = "effect.video.enhance_method";
/// See [`DENOISE`]: the switch of the [`HDR`] stage.
pub const HDR_LOOK: &str = "effect.video.hdr";
/// The lighting group's settings.
pub const RELIGHT: &str = "effect.lighting.relight";
/// See [`RELIGHT`].
pub const LIGHT: &str = "effect.lighting.light";
/// See [`RELIGHT`].
pub const DIRECTION: &str = "effect.lighting.direction";
/// See [`RELIGHT`].
pub const BRIGHTNESS: &str = "effect.lighting.brightness";

/// The range and step of the relighting direction, in degrees.
pub const DIRECTION_RANGE: (i64, i64, i64) = (-90, 90, 5);
/// The range and step of the relighting brightness, in percent.
pub const BRIGHTNESS_RANGE: (i64, i64, i64) = (50, 200, 5);
/// The default relighting brightness, in percent.
pub const DEFAULT_BRIGHTNESS: i64 = 100;

/// The keys of `_host`, which the daemon fills in.
#[derive(Debug, Default, Deserialize)]
struct HostKeys {
    #[serde(default)]
    compute_capability: Option<(u32, u32)>,
}

/// The module's configuration.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    sdk_root: Option<PathBuf>,
    #[serde(default, rename = "_host")]
    host: HostKeys,
}

/// The settings of the module: the cut-out group (the chair, a parameter
/// of the daemon's background effect), the video group (noise removal and
/// enhancement) and the lighting group, as the daemon's settings model
/// describes them.
pub fn settings() -> SettingsDescription {
    let (dmin, dmax, dstep) = DIRECTION_RANGE;
    let (bmin, bmax, bstep) = BRIGHTNESS_RANGE;
    let lit = |s: Setting| {
        s.stage(StageId::RELIGHT.as_str())
            .enabled_when(RELIGHT, Op::Eq, true)
            .disabled_reason(REASON_TURN_ON_RELIGHT)
    };
    let light_label = |l: RelightLight| match l {
        RelightLight::Warm => "Warm",
        RelightLight::Neutral => "Neutral",
        RelightLight::Cool => "Cool",
    };
    let method_label = |m: EnhanceMethod| match m {
        EnhanceMethod::Ai => "AI super resolution",
        EnhanceMethod::Fast => "Fast upscale (lower power)",
    };
    SettingsDescription::new()
        .group(
            Group::new("cutout", "Cut-out").setting(
                Setting::bool(CHAIR, "Hide the chair with the background", false)
                    .stage(StageId::BACKGROUND.as_str())
                    .key(BACKGROUND_CHAIR)
                    .apply(Apply::Reload)
                    .enabled_when(DAEMON_BACKGROUND_MODE, Op::Ne, "none")
                    .disabled_reason(REASON_TURN_ON_BACKGROUND)
                    .hint("switch"),
            ),
        )
        .group(
            Group::new("video", "Video")
                .setting(
                    Setting::bool(DENOISE, "Noise removal", false)
                        .stage(StageId::DENOISE.as_str())
                        .role(Role::Enable)
                        .hint("switch"),
                )
                .setting(
                    Setting::bool(ENHANCE, "Enhance low-resolution camera", false)
                        .stage(StageId::ENHANCE.as_str())
                        .role(Role::Enable)
                        .hint("switch"),
                )
                .setting(
                    Setting::enumeration(
                        ENHANCE_METHOD_SETTING,
                        "Enhancement method",
                        EnhanceMethod::ALL.map(|m| (m.as_str(), method_label(m))),
                        EnhanceMethod::default().as_str(),
                    )
                    .stage(StageId::ENHANCE.as_str())
                    .key(ENHANCE_METHOD)
                    .apply(Apply::Reload)
                    .enabled_when(ENHANCE, Op::Eq, true)
                    .disabled_reason(REASON_TURN_ON_ENHANCE)
                    .hint("segmented"),
                )
                .setting(
                    Setting::bool(HDR_LOOK, "HDR look", false)
                        .stage(HDR)
                        .role(Role::Enable)
                        .hint("switch"),
                ),
        )
        .group(
            Group::new("lighting", "Lighting")
                .setting(
                    Setting::bool(RELIGHT, "Relighting", false)
                        .stage(StageId::RELIGHT.as_str())
                        .role(Role::Enable)
                        .hint("switch"),
                )
                .setting(
                    lit(Setting::enumeration(
                        LIGHT,
                        "Light",
                        RelightLight::ALL.map(|l| (l.as_str(), light_label(l))),
                        RelightLight::Neutral.as_str(),
                    ))
                    .key(RELIGHT_LIGHT)
                    .apply(Apply::Reload)
                    .hint("segmented"),
                )
                .setting(
                    lit(Setting::int(
                        DIRECTION,
                        "Light direction",
                        dmin,
                        dmax,
                        dstep,
                        0,
                    ))
                    .unit("°")
                    .key(RELIGHT_DIRECTION)
                    .apply(Apply::Live)
                    .hint("slider"),
                )
                .setting(
                    lit(Setting::int(
                        BRIGHTNESS,
                        "Brightness",
                        bmin,
                        bmax,
                        bstep,
                        DEFAULT_BRIGHTNESS,
                    ))
                    .unit("%")
                    .key(RELIGHT_BRIGHTNESS)
                    .apply(Apply::Live)
                    .hint("slider"),
                ),
        )
}

/// The probe of the SDK tree at `root` for a GPU of `compute_capability`
/// (`None`: any): every stage (those of the effect backend, then [`HDR`]
/// and [`FRAME_GENERATION`]), with [`REASON_NOT_INSTALLED`] for those not
/// installed, and the detail of each of those (`stage: why`). With no root,
/// every stage is available (nothing can be known). Reads directories only.
pub fn probe_root(
    root: Option<&Path>,
    compute_capability: Option<(u32, u32)>,
) -> (ProbeReport, Vec<String>) {
    let mut report = ProbeReport::new();
    let mut details = Vec::new();
    let Some(root) = root else {
        for s in STAGES {
            report = report.available(s.as_str());
        }
        return (report.available(HDR).available(FRAME_GENERATION), details);
    };
    let features = feature_availability(root, compute_capability);
    let background = stage_availability_of(&features, StageId::BACKGROUND.as_str());
    for s in STAGES {
        let mut a = stage_availability_of(&features, s.as_str());
        if s == StageId::RELIGHT
            && a.is_installed()
            && let Some(why) = background.reason()
        {
            // It relights through the cut-out, which cannot run.
            a = obc_backend::Availability::NotInstalled(format!(
                "needs the background stage, which is not installed: {why}"
            ));
        }
        report = match a.reason() {
            None => report.available(s.as_str()),
            Some(why) => {
                details.push(format!("{s}: {why}"));
                report.unavailable(s.as_str(), REASON_NOT_INSTALLED)
            }
        };
    }
    // The stages of one feature each.
    for (stage, feature) in [
        (HDR, Feature::TrueHdr),
        (FRAME_GENERATION, Feature::VideoFrameGeneration),
    ] {
        let why = features
            .iter()
            .find(|(f, _)| *f == feature)
            .and_then(|(_, a)| a.reason().map(str::to_string));
        report = match why {
            None => report.available(stage),
            Some(why) => {
                details.push(format!("{stage}: {why}"));
                report.unavailable(stage, REASON_NOT_INSTALLED)
            }
        };
    }
    (report, details)
}

/// The module object.
pub struct MaxineModule {
    effects: MaxineFactory,
    enhance: MaxineVsrFactory,
    framegen: MaxineVfgFactory,
    truehdr: MaxineTrueHdrFactory,
    compute_capability: Option<(u32, u32)>,
}

impl Module for MaxineModule {
    type Instance = MaxineInstance;

    fn init(config: serde_json::Value) -> Result<Self, Error> {
        let config: Config = if config.is_null() {
            Config::default()
        } else {
            serde_json::from_value(config)?
        };
        let root = config.sdk_root.filter(|r| !r.as_os_str().is_empty());
        // The process-wide configured root is this module's: the daemon
        // links no Maxine code. MAXINE_VFX_ROOT still takes precedence.
        obc_backend_maxine::set_configured_root(root);
        let effects = MaxineFactory::from_env();
        let enhance = MaxineVsrFactory::from_env();
        let framegen = MaxineVfgFactory::from_env();
        let truehdr = MaxineTrueHdrFactory::from_env();
        match effects.root() {
            Some(r) => log::debug!("the Maxine SDK root is {}", r.display()),
            None => log::debug!("no Maxine SDK root: the library search path is used"),
        }
        Ok(MaxineModule {
            effects,
            enhance,
            framegen,
            truehdr,
            compute_capability: config.host.compute_capability,
        })
    }

    fn probe(&self) -> ProbeReport {
        let root = obc_backend_maxine::usable_root();
        let (report, details) = probe_root(root.as_deref(), self.compute_capability);
        match &root {
            None => log::debug!("no Maxine SDK root: which effects are installed is unknown"),
            Some(r) => {
                for d in details {
                    log::info!("not installed in {}: {d}", r.display());
                }
            }
        }
        report
    }

    fn settings(&self) -> SettingsDescription {
        settings()
    }

    /// Opens the SDK as the built-in factories' probes did: the effects'
    /// libraries and model directory for `background`, `denoise` and
    /// `relight`, the upscaler's libraries for `enhance`, the SDK's for
    /// `frame-generation` and `hdr`. No CUDA context, no model.
    fn check(&self, stages: &[&str]) -> Result<(), Error> {
        let effect = |s: &&str| {
            [StageId::BACKGROUND, StageId::DENOISE, StageId::RELIGHT]
                .iter()
                .any(|id| id == *s)
        };
        if stages.iter().any(effect) {
            BackendFactory::probe(&self.effects).map_err(module_error)?;
        }
        if stages.contains(&StageId::ENHANCE.as_str()) {
            EnhancerFactory::probe(&self.enhance).map_err(module_error)?;
        }
        if stages.contains(&FRAME_GENERATION) {
            self.framegen.probe().map_err(module_error)?;
        }
        if stages.contains(&HDR) {
            self.truehdr.probe().map_err(module_error)?;
        }
        Ok(())
    }

    fn create_instance(&self) -> Result<MaxineInstance, Error> {
        Ok(MaxineInstance {
            effects: self.effects.clone(),
            enhance: self.enhance.clone(),
            framegen: self.framegen.clone(),
            compute_capability: self.compute_capability,
            chain: Chain::Empty,
            gpu: None,
            blur_strength: FrameParams::default().blur_strength,
            live: BTreeMap::new(),
            vfg: None,
            vfg_in_chain: false,
            fg: FrameGen::default(),
            truehdr: self.truehdr.clone(),
            hdr: None,
            hdr_in_chain: false,
        })
    }
}

/// What an instance holds.
enum Chain {
    /// Nothing created yet.
    Empty,
    /// The effect backend (loaded or not).
    Effect(Box<MaxineBackend>),
    /// The upscaler (loaded or not).
    Enhance(Box<MaxineVsr>),
    /// No stage but frame generation or the HDR look is loaded: the frame
    /// goes to them as it came.
    Passthrough,
}

/// One chain of the module's stages.
pub struct MaxineInstance {
    effects: MaxineFactory,
    enhance: MaxineVsrFactory,
    /// The GPU's, as the module's configuration gives it.
    compute_capability: Option<(u32, u32)>,
    chain: Chain,
    /// The context and stream of the last load.
    gpu: Option<GpuContext>,
    /// The last blur strength a frame gave.
    blur_strength: f32,
    /// The last live parameters frames gave, by stage.
    live: BTreeMap<StageId, Params>,
    framegen: MaxineVfgFactory,
    /// The frame generation effect, once one was loaded.
    vfg: Option<MaxineVfg>,
    /// Frame generation is in the loaded chain (`vfg` is loaded).
    vfg_in_chain: bool,
    /// What frame generation holds.
    fg: FrameGen,
    truehdr: MaxineTrueHdrFactory,
    /// The HDR look, once one was loaded.
    hdr: Option<MaxineTrueHdr>,
    /// The HDR look is in the loaded chain (`hdr` is loaded).
    hdr_in_chain: bool,
}

/// The module's error for a backend's.
fn module_error(e: BackendError) -> Error {
    match e {
        BackendError::Stage { stage, message, .. } => Error::stage(stage.as_str(), message),
        BackendError::Unavailable(m) => Error::unavailable(m),
        BackendError::InvalidInput(m) => Error::invalid(m),
        BackendError::Load { message, .. } | BackendError::Run { message, .. } => {
            Error::failed(message)
        }
    }
}

/// The stage error of frame generation for a backend's error: frame
/// generation alone fails, and the rest of the chain goes on without it.
fn framegen_error(e: BackendError) -> Error {
    Error::stage(FRAME_GENERATION, e.to_string())
}

/// The stage error of the HDR look for a backend's error: the HDR look
/// alone fails, and the rest of the chain goes on without it.
fn hdr_error(e: BackendError) -> Error {
    Error::stage(HDR, e.to_string())
}

fn value(v: &Value<'_>) -> BackendValue {
    match v {
        Value::Bool(b) => BackendValue::Bool(*b),
        Value::Int(i) => BackendValue::Int(*i),
        Value::Float(f) => BackendValue::Float(*f),
        Value::Str(s) => BackendValue::Str((*s).to_string()),
    }
}

fn gpu_of(handles: obc_module_sdk::CudaHandles) -> Result<GpuContext, Error> {
    if handles.is_null() {
        return Err(Error::invalid(
            "a Maxine chain needs the frames' CUDA context",
        ));
    }
    Ok(GpuContext {
        context: handles.context,
        stream: handles.stream,
    })
}

fn colour(req: &ChainRequest<'_>) -> Result<ColourSpec, Error> {
    let matrix = match req.matrix() {
        ColourMatrix::Bt601 => BackendMatrix::Bt601,
        ColourMatrix::Bt709 => BackendMatrix::Bt709,
        other => {
            return Err(Error::unsupported(format!(
                "the Maxine module converts BT.601 and BT.709 frames, not {other:?}"
            )));
        }
    };
    Ok(ColourSpec {
        matrix,
        full_range: req.full_range(),
    })
}

/// Plane `i`'s address and pitch.
fn plane(f: &Frame<'_>, i: usize) -> Result<(u64, usize), Error> {
    let p = f
        .plane(i)
        .ok_or_else(|| Error::invalid(format!("the frame has no plane {i}")))?;
    Ok((p.ptr_or_fd + p.offset, p.pitch as usize))
}

/// A CUDA NV12 frame as the backend describes one.
fn device_nv12(f: &Frame<'_>) -> Result<DeviceNv12, Error> {
    if f.kind() != FrameKind::Cuda || f.format() != PixelFormat::Nv12 {
        return Err(Error::unsupported(
            "the Maxine effects take CUDA NV12 frames only",
        ));
    }
    let (y, y_pitch) = plane(f, 0)?;
    let (uv, uv_pitch) = plane(f, 1)?;
    Ok(DeviceNv12 {
        y,
        y_pitch,
        uv,
        uv_pitch,
        width: f.width(),
        height: f.height(),
    })
}

/// A CUDA NV12 or I420 frame as the enhance seam describes one.
fn device_yuv420(f: &Frame<'_>) -> Result<DeviceYuv420, Error> {
    if f.kind() != FrameKind::Cuda {
        return Err(Error::unsupported(
            "the Maxine upscaler takes CUDA frames only",
        ));
    }
    let (y, y_pitch) = plane(f, 0)?;
    let (u, u_pitch) = plane(f, 1)?;
    let (layout, v, v_pitch) = match f.format() {
        PixelFormat::Nv12 => (YuvLayout::Nv12, u + 1, u_pitch),
        PixelFormat::I420 => {
            let (v, v_pitch) = plane(f, 2)?;
            (YuvLayout::I420, v, v_pitch)
        }
        other => {
            return Err(Error::unsupported(format!(
                "the Maxine upscaler reads NV12 or I420, not {other:?}"
            )));
        }
    };
    Ok(DeviceYuv420 {
        layout,
        y,
        y_pitch,
        u,
        u_pitch,
        v,
        v_pitch,
        width: f.width(),
        height: f.height(),
    })
}

/// A copy of a host BGR frame.
fn host_bgr(f: &Frame<'_>) -> Result<HostBgr, Error> {
    if f.kind() != FrameKind::Host || f.format() != PixelFormat::Bgr {
        return Err(Error::invalid(
            "the background image must be a host BGR frame",
        ));
    }
    let (rows, row_bytes) = f
        .plane_geometry(0)
        .ok_or_else(|| Error::invalid("the background image has no plane"))?;
    let pitch = f.plane(0).map_or(0, |p| p.pitch as usize);
    // SAFETY: the host owns the background frame for the call; only read.
    let src = unsafe { f.host_plane(0) }?;
    let mut data = Vec::with_capacity(rows * row_bytes);
    for r in 0..rows {
        data.extend_from_slice(&src[r * pitch..r * pitch + row_bytes]);
    }
    Ok(HostBgr {
        width: f.width(),
        height: f.height(),
        stride: row_bytes,
        data,
    })
}

/// The load-time parameters of the effect backend's stages from the
/// request's: each stage's by key, the `background` stage's but its
/// `mode` (which is the effect), none of frame generation's or the HDR
/// look's. A `chair`
/// that is not a boolean is refused: it is the matte's, which never fails
/// open.
fn effect_params(params: &[Param<'_>]) -> Result<BTreeMap<StageId, Params>, Error> {
    let mut out: BTreeMap<StageId, Params> = BTreeMap::new();
    for p in params {
        if p.stage == FRAME_GENERATION || p.stage == HDR {
            continue;
        }
        if p.stage == StageId::BACKGROUND.as_str() {
            if p.key == BACKGROUND_MODE {
                continue;
            }
            if p.key == BACKGROUND_CHAIR && p.value.as_bool().is_none() {
                return Err(Error::invalid(format!(
                    "the background stage's {BACKGROUND_CHAIR} must be a boolean, not {:?}",
                    p.value
                )));
            }
        }
        let stage = StageId::new(p.stage).map_err(|e| Error::invalid(e.to_string()))?;
        out.entry(stage)
            .or_default()
            .insert(p.key.to_string(), value(&p.value));
    }
    Ok(out)
}

/// The enhance stage's method the request asks for ([`ENHANCE_METHOD`]:
/// `ai` or `fast`); `None` when it asks for none.
fn enhance_method(params: &[Param<'_>]) -> Result<Option<EnhanceMethod>, Error> {
    match Param::find(params, StageId::ENHANCE.as_str(), ENHANCE_METHOD) {
        None => Ok(None),
        Some(Value::Str(s)) => s.parse().map(Some).map_err(Error::invalid),
        Some(other) => Err(Error::invalid(format!(
            "the enhance stage's {ENHANCE_METHOD} must be ai or fast, not {other:?}"
        ))),
    }
}

/// Whether `method`'s feature is installed in the SDK tree at `root`, for
/// a GPU of `compute_capability`: the stage is offered when either method
/// is, so a load of the other one is refused here, with the reason. With
/// no root nothing can be known, and the load decides.
fn method_installed(
    root: Option<&Path>,
    compute_capability: Option<(u32, u32)>,
    method: EnhanceMethod,
) -> Result<(), Error> {
    let Some(root) = root else {
        return Ok(());
    };
    let feature = method.feature();
    let why = feature_availability(root, compute_capability)
        .into_iter()
        .find(|(f, _)| *f == feature)
        .and_then(|(_, a)| a.reason().map(str::to_string));
    match why {
        None => Ok(()),
        Some(why) => Err(Error::unavailable(format!(
            "the enhance method {method} needs the {feature} feature, which is not installed: \
             {why}"
        ))),
    }
}

/// The method an enhance load runs, for the SDK tree at `root` and a GPU
/// of `compute_capability`. A requested `fast` runs when its feature is
/// installed, else is refused with the reason (never replaced by the
/// costlier `ai`). A requested `ai` runs when installed; when its feature
/// is missing but the upscale is installed, `fast` runs instead and a
/// warning names why, since the daemon always sends the method (the default
/// `ai` when the user chose none). With none requested, `ai` when it is
/// installed, else `fast` when that is; neither installed is refused with
/// the preferred method's reason. With no root nothing can be known: the
/// requested method, or `ai`.
fn resolve_enhance_method(
    params: &[Param<'_>],
    root: Option<&Path>,
    compute_capability: Option<(u32, u32)>,
) -> Result<EnhanceMethod, Error> {
    let requested = enhance_method(params)?;
    let preferred = requested.unwrap_or_default();
    match method_installed(root, compute_capability, preferred) {
        Ok(()) => Ok(preferred),
        Err(e) if requested == Some(EnhanceMethod::Fast) => Err(e),
        Err(e) => {
            let fast = EnhanceMethod::Fast;
            if method_installed(root, compute_capability, fast).is_ok() {
                log::warn!("running the enhance method {fast} instead of {preferred}: {e:?}");
                Ok(fast)
            } else {
                Err(e)
            }
        }
    }
}

impl MaxineInstance {
    /// Loads the effect backend for `stages`, the request's stages but
    /// frame generation.
    fn load_effect(
        &mut self,
        req: &ChainRequest<'_>,
        stages: &[&str],
        gpu: GpuContext,
    ) -> Result<LoadReport, Error> {
        if req.input_format() != PixelFormat::Nv12
            || req.output_format() != PixelFormat::Nv12
            || req.input_size() != req.output_size()
        {
            return Err(Error::unsupported(
                "the Maxine effects run on NV12 frames, in place of the same size",
            ));
        }
        let effect = if req.has_stage(StageId::BACKGROUND.as_str()) {
            match req
                .param(StageId::BACKGROUND.as_str(), BACKGROUND_MODE)
                .and_then(|v| v.as_str())
            {
                Some("blur") => EffectKind::Blur,
                Some("replace") => EffectKind::Replace,
                other => {
                    return Err(Error::invalid(format!(
                        "the background stage's mode must be blur or replace, not {other:?}"
                    )));
                }
            }
        } else {
            EffectKind::None
        };
        let mut ids = Vec::new();
        for s in stages {
            match *s {
                "background" => {}
                "denoise" | "relight" => {
                    ids.push(StageId::new(*s).map_err(|e| Error::invalid(e.to_string()))?)
                }
                other => {
                    return Err(Error::invalid(format!(
                        "the Maxine module has no stage {other:?} for the effect chain"
                    )));
                }
            }
        }
        let stages = StageList::new(ids).map_err(|e| Error::invalid(e.to_string()))?;
        let params = effect_params(req.params())?;
        let background = match (effect, req.background()) {
            (EffectKind::Replace, Some(f)) => Some(std::sync::Arc::new(host_bgr(f)?)),
            _ => None,
        };
        let (width, height) = req.input_size();
        let load = LoadRequest {
            effect,
            stages,
            width,
            height,
            colour: colour(req)?,
            background,
            params,
        };
        if !matches!(self.chain, Chain::Effect(_)) {
            self.unload_chain();
            let backend = self.effects.create_maxine().map_err(module_error)?;
            self.chain = Chain::Effect(Box::new(backend));
        }
        let Chain::Effect(backend) = &mut self.chain else {
            unreachable!("set above");
        };
        let report = backend.load(&gpu, &load).map_err(module_error)?;
        Ok(LoadReport {
            load_ms: report.load_ms as f32,
            warmup_ms: report.warmup_ms.map(|w| w as f32),
            reused_model: report.reused_model,
            notice: None,
        })
    }

    fn load_enhance(
        &mut self,
        req: &ChainRequest<'_>,
        gpu: GpuContext,
    ) -> Result<LoadReport, Error> {
        let layout = match req.input_format() {
            PixelFormat::Nv12 => YuvLayout::Nv12,
            PixelFormat::I420 => YuvLayout::I420,
            other => {
                return Err(Error::unsupported(format!(
                    "the Maxine upscaler reads NV12 or I420, not {other:?}"
                )));
            }
        };
        let ((iw, ih), (ow, oh)) = (req.input_size(), req.output_size());
        if req.output_format() != PixelFormat::Nv12
            || iw == 0
            || ih == 0
            || !ow.is_multiple_of(iw)
            || !oh.is_multiple_of(ih)
            || ow / iw != oh / ih
        {
            return Err(Error::unsupported(format!(
                "the Maxine upscaler writes NV12 an integer factor larger; {iw}x{ih} to \
                 {ow}x{oh} is not that"
            )));
        }
        let chroma = match req.chroma_siting() {
            ChromaSiting::Mpeg2 => EnhanceSiting::Cosited,
            ChromaSiting::Jpeg => EnhanceSiting::Interstitial,
            ChromaSiting::Cosited => EnhanceSiting::TopLeft,
            ChromaSiting::Other(c) => {
                return Err(Error::unsupported(format!("unknown chroma siting {c}")));
            }
        };
        let enhance = EnhanceRequest {
            width: iw,
            height: ih,
            scale: ow / iw,
            layout,
            colour: colour(req)?,
            chroma,
            // The method, the stage's one parameter, is given to the load.
            params: Params::new(),
        };
        let method = resolve_enhance_method(
            req.params(),
            obc_backend_maxine::usable_root().as_deref(),
            self.compute_capability,
        )?;
        if !matches!(self.chain, Chain::Enhance(_)) {
            self.unload_chain();
            let vsr = self.enhance.create_vsr().map_err(module_error)?;
            self.chain = Chain::Enhance(Box::new(vsr));
        }
        let Chain::Enhance(vsr) = &mut self.chain else {
            unreachable!("set above");
        };
        let report = vsr
            .load_with(&gpu, &enhance, method)
            .map_err(module_error)?;
        Ok(LoadReport {
            load_ms: report.load_ms as f32,
            warmup_ms: report.warmup_ms.map(|w| w as f32),
            reused_model: report.reused_model,
            notice: None,
        })
    }

    /// Loads frame generation for the request's frames, unless the loaded
    /// one already fits them. Changes nothing of the instance: the effect
    /// is returned, to be kept once the rest of the chain has loaded too.
    /// `None`: the loaded one is kept.
    fn load_framegen(
        &self,
        req: &ChainRequest<'_>,
        gpu: GpuContext,
    ) -> Result<Option<(MaxineVfg, LoadReport)>, Error> {
        if req.input_format() != PixelFormat::Nv12
            || req.output_format() != PixelFormat::Nv12
            || req.input_size() != req.output_size()
        {
            return Err(Error::unsupported(
                "frame generation runs on NV12 frames, in place of the same size",
            ));
        }
        let (width, height) = req.input_size();
        let colour = colour(req)?;
        if self
            .vfg
            .as_ref()
            .is_some_and(|v| v.is_loaded_for(&gpu, width, height, colour))
        {
            return Ok(None);
        }
        let mut vfg = self.framegen.create_vfg().map_err(framegen_error)?;
        let report = vfg
            .load(&gpu, width, height, colour)
            .map_err(framegen_error)?;
        Ok(Some((
            vfg,
            LoadReport {
                load_ms: report.load_ms as f32,
                warmup_ms: None,
                reused_model: false,
                notice: None,
            },
        )))
    }

    /// Loads the HDR look for the request's frames, unless the loaded one
    /// already fits them, as [`Self::load_framegen`]: changes nothing of
    /// the instance. Every failure is the stage's.
    fn load_hdr(
        &self,
        req: &ChainRequest<'_>,
        gpu: GpuContext,
    ) -> Result<Option<(MaxineTrueHdr, LoadReport)>, Error> {
        if req.input_format() != PixelFormat::Nv12
            || req.output_format() != PixelFormat::Nv12
            || req.input_size() != req.output_size()
        {
            return Err(Error::stage(
                HDR,
                "the HDR look runs on NV12 frames, in place of the same size",
            ));
        }
        let (width, height) = req.input_size();
        let colour = colour(req).map_err(|e| Error::stage(HDR, e.wire_message()))?;
        if self
            .hdr
            .as_ref()
            .is_some_and(|h| h.is_loaded_for(&gpu, width, height, colour))
        {
            return Ok(None);
        }
        let mut hdr = self.truehdr.create_truehdr().map_err(hdr_error)?;
        let report = hdr.load(&gpu, width, height, colour).map_err(hdr_error)?;
        Ok(Some((
            hdr,
            LoadReport {
                load_ms: report.load_ms as f32,
                warmup_ms: None,
                reused_model: false,
                notice: None,
            },
        )))
    }

    fn unload_chain(&mut self) {
        match &mut self.chain {
            Chain::Empty | Chain::Passthrough => {}
            Chain::Effect(b) => b.unload(),
            Chain::Enhance(v) => v.unload(),
        }
    }

    /// Runs the chain but frame generation on one frame: the other stages,
    /// then the HDR look when it is in the chain, on every frame (repeats
    /// included). A failure of the HDR look drops it from the chain; the
    /// output then holds what the other stages wrote (or what the look had
    /// written of it), and the host runs the frame again without it.
    fn process_stages(
        &mut self,
        gpu: &GpuContext,
        input: &Frame<'_>,
        output: &Frame<'_>,
    ) -> Result<(), Error> {
        if !self.hdr_in_chain {
            return self.process_others(gpu, input, output);
        }
        let dst = device_nv12(output)?;
        let src = if matches!(self.chain, Chain::Passthrough) {
            // Without another stage the HDR look reads the input itself.
            device_nv12(input)?
        } else {
            self.process_others(gpu, input, output)?;
            dst
        };
        let look = self
            .hdr
            .as_mut()
            .ok_or_else(|| Error::state("the HDR look is not loaded"))?;
        if let Err(e) = look.process(gpu, &src, &dst) {
            // The HDR look alone failed: the rest goes on without it.
            self.hdr_in_chain = false;
            look.unload();
            return Err(hdr_error(e));
        }
        Ok(())
    }

    /// Runs the chain but frame generation and the HDR look on one frame.
    fn process_others(
        &mut self,
        gpu: &GpuContext,
        input: &Frame<'_>,
        output: &Frame<'_>,
    ) -> Result<(), Error> {
        match &mut self.chain {
            Chain::Empty => Err(Error::state("nothing is loaded")),
            Chain::Effect(backend) => {
                let params = FrameParams {
                    blur_strength: self.blur_strength,
                    live: self.live.clone(),
                };
                backend
                    .process(gpu, &params, &device_nv12(input)?, &device_nv12(output)?)
                    .map_err(module_error)
            }
            Chain::Enhance(vsr) => vsr
                .process(gpu, &device_yuv420(input)?, &device_nv12(output)?)
                .map_err(module_error),
            Chain::Passthrough => {
                let (src, dst) = (device_nv12(input)?, device_nv12(output)?);
                if input.same_memory(output) {
                    return Ok(());
                }
                // SAFETY: the host's frames, of the current context, for the
                // call; distinct (not the same memory).
                unsafe { obc_gpu::copy_nv12_async(&src, &dst, gpu.stream) }
                    .map_err(|e| Error::failed(e.to_string()))
            }
        }
    }

    /// Runs one frame through the chain with frame generation (see
    /// [`framegen`]). A failure of frame generation drops it from the
    /// chain.
    fn process_framegen(
        &mut self,
        gpu: &GpuContext,
        input: &Frame<'_>,
        output: &Frame<'_>,
    ) -> Result<(), Error> {
        let step = self.fg.step(input.is_repeat());
        let dst = device_nv12(output)?;
        if step.runs_stages() {
            self.process_stages(gpu, input, output)?;
        }
        let vfg = self
            .vfg
            .as_mut()
            .ok_or_else(|| Error::state("frame generation is not loaded"))?;
        let result = match step {
            Step::Held => vfg.write_held(gpu, &dst),
            Step::First => vfg.push(gpu, &dst),
            Step::Interpolate => vfg
                .push(gpu, &dst)
                .and_then(|()| vfg.interpolate(gpu, &dst)),
        };
        if let Err(e) = result {
            // Frame generation alone failed: the rest goes on without it.
            self.vfg_in_chain = false;
            self.fg.reset();
            if let Some(v) = self.vfg.as_mut() {
                v.unload();
            }
            return Err(framegen_error(e));
        }
        self.fg.done(step);
        Ok(())
    }
}

impl Instance for MaxineInstance {
    fn load(&mut self, req: &ChainRequest<'_>) -> Result<LoadReport, Error> {
        if req.input_kind() != FrameKind::Cuda {
            return Err(Error::unsupported(
                "the Maxine module runs on CUDA frames only",
            ));
        }
        let gpu = gpu_of(req.cuda())?;
        if req.has_stage(StageId::ENHANCE.as_str()) && req.stages().len() != 1 {
            return Err(Error::unsupported(
                "the enhance stage runs alone in its chain",
            ));
        }
        let wants_framegen = req.has_stage(FRAME_GENERATION);
        let wants_hdr = req.has_stage(HDR);
        let others: Vec<&str> = req
            .stages()
            .iter()
            .copied()
            .filter(|s| *s != FRAME_GENERATION && *s != HDR)
            .collect();
        // Frame generation and the HDR look first, so that a failure of
        // either leaves everything as it was: the host loads again without
        // it.
        let framegen = if wants_framegen {
            self.load_framegen(req, gpu)?
        } else {
            None
        };
        let hdr = if wants_hdr {
            self.load_hdr(req, gpu)?
        } else {
            None
        };
        let result = if others.is_empty() {
            self.unload_chain();
            self.chain = Chain::Passthrough;
            Ok(LoadReport {
                load_ms: 0.0,
                warmup_ms: None,
                reused_model: framegen.is_none() && hdr.is_none(),
                notice: None,
            })
        } else if req.has_stage(StageId::ENHANCE.as_str()) {
            self.load_enhance(req, gpu)
        } else {
            self.load_effect(req, &others, gpu)
        };
        let mut result = result?;
        match framegen {
            Some((vfg, report)) => {
                if let Some(mut old) = self.vfg.replace(vfg) {
                    old.unload();
                }
                result.load_ms += report.load_ms;
                result.reused_model = false;
            }
            None if !wants_framegen => {
                if let Some(v) = self.vfg.as_mut() {
                    v.unload();
                }
            }
            None => {}
        }
        self.vfg_in_chain = wants_framegen;
        match hdr {
            Some((look, report)) => {
                if let Some(mut old) = self.hdr.replace(look) {
                    old.unload();
                }
                result.load_ms += report.load_ms;
                result.reused_model = false;
            }
            None if !wants_hdr => {
                if let Some(h) = self.hdr.as_mut() {
                    h.unload();
                }
            }
            None => {}
        }
        self.hdr_in_chain = wants_hdr;
        // A new chain has no previous frame.
        self.fg.reset();
        self.gpu = Some(gpu);
        // The live parameters of the previous chain do not carry over:
        // each frame gives those in force.
        self.live.clear();
        self.blur_strength = FrameParams::default().blur_strength;
        Ok(result)
    }

    fn process(
        &mut self,
        input: &Frame<'_>,
        output: &Frame<'_>,
        live: &[Param<'_>],
    ) -> Result<(), Error> {
        let gpu = gpu_of(input.cuda())?;
        for p in live {
            if p.stage == StageId::BACKGROUND.as_str() {
                if p.key == BACKGROUND_BLUR_STRENGTH
                    && let Some(s) = p.value.as_float()
                {
                    self.blur_strength = s as f32;
                }
                continue;
            }
            let stage = StageId::new(p.stage).map_err(|e| Error::invalid(e.to_string()))?;
            self.live
                .entry(stage)
                .or_default()
                .insert(p.key.to_string(), value(&p.value));
        }
        if self.vfg_in_chain {
            self.process_framegen(&gpu, input, output)
        } else {
            self.process_stages(&gpu, input, output)
        }
    }

    const LIVE_BACKGROUND: bool = true;

    /// Hands the frame to the effect backend, which converts it into the
    /// background image of its composite on the chain's stream
    /// ([`MaxineBackend`]'s `set_background_frame`).
    fn set_background(&mut self, frame: Option<&Frame<'_>>) -> Result<(), Error> {
        let Chain::Effect(backend) = &mut self.chain else {
            return Err(Error::invalid(
                "a per-frame background needs the background stage under replace",
            ));
        };
        let frame = match frame {
            None => None,
            Some(f) if f.kind() != FrameKind::Cuda => {
                return Err(Error::invalid(
                    "the Maxine module takes CUDA background frames only",
                ));
            }
            Some(f) => Some(BackgroundFrame::Cuda(device_nv12(f)?)),
        };
        backend.set_background_frame(frame).map_err(module_error)
    }

    fn reset_temporal(&mut self) -> Result<Vec<String>, Error> {
        // The next frame has no previous one. The HDR look has no temporal
        // state (`obc_backend_maxine::truehdr`).
        self.fg.reset();
        let (Chain::Effect(backend), Some(gpu)) = (&mut self.chain, &self.gpu) else {
            return Ok(Vec::new());
        };
        let dropped = backend.reset_temporal(gpu).map_err(module_error)?;
        Ok(dropped.iter().map(|s| s.to_string()).collect())
    }

    fn unload(&mut self) {
        self.unload_chain();
        if matches!(self.chain, Chain::Passthrough) {
            self.chain = Chain::Empty;
        }
        if let Some(v) = self.vfg.as_mut() {
            v.unload();
        }
        self.vfg_in_chain = false;
        self.fg.reset();
        if let Some(h) = self.hdr.as_mut() {
            h.unload();
        }
        self.hdr_in_chain = false;
        self.gpu = None;
        self.live.clear();
        self.blur_strength = FrameParams::default().blur_strength;
    }

    fn timings(&self) -> Vec<(String, f32)> {
        match &self.chain {
            Chain::Empty | Chain::Passthrough => Vec::new(),
            Chain::Effect(b) => b.timings().map(|t| t.steps().to_vec()).unwrap_or_default(),
            Chain::Enhance(v) => v
                .timings()
                .map(|t| {
                    vec![
                        (StageTimings::CONVERT_IN.to_string(), t.convert_in_ms),
                        (StageId::ENHANCE.to_string(), t.upscale_ms),
                        (StageTimings::CONVERT_OUT.to_string(), t.convert_out_ms),
                    ]
                })
                .unwrap_or_default(),
        }
    }
}

obc_module_sdk::export_module!(MaxineModule);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_settings_are_the_cutout_video_and_lighting_groups() {
        let doc: serde_json::Value = serde_json::from_str(&settings().to_json()).unwrap();
        let expected = serde_json::json!({"groups":[
            {"id":"cutout","label":"Cut-out","settings":[
                {"id":"effect.cutout.chair","label":"Hide the chair with the background",
                 "type":"bool","default":false,"stage":"background","key":"chair",
                 "apply":"reload","enabled_when":[["effect.background.mode","ne","none"]],
                 "disabled_reason":"Turn on the background effect to change this",
                 "hint":"switch"}]},
            {"id":"video","label":"Video","settings":[
                {"id":"effect.video.denoise","label":"Noise removal","type":"bool","default":false,
                 "stage":"denoise","role":"enable","hint":"switch"},
                {"id":"effect.video.enhance","label":"Enhance low-resolution camera","type":"bool",
                 "default":false,"stage":"enhance","role":"enable","hint":"switch"},
                {"id":"effect.video.enhance_method","label":"Enhancement method","type":"enum",
                 "options":[["ai","AI super resolution"],["fast","Fast upscale (lower power)"]],
                 "default":"ai","stage":"enhance","key":"method","apply":"reload",
                 "enabled_when":[["effect.video.enhance","eq",true]],
                 "disabled_reason":"Turn on enhancement to change this","hint":"segmented"},
                {"id":"effect.video.hdr","label":"HDR look","type":"bool","default":false,
                 "stage":"hdr","role":"enable","hint":"switch"}]},
            {"id":"lighting","label":"Lighting","settings":[
                {"id":"effect.lighting.relight","label":"Relighting","type":"bool","default":false,
                 "stage":"relight","role":"enable","hint":"switch"},
                {"id":"effect.lighting.light","label":"Light","type":"enum",
                 "options":[["warm","Warm"],["neutral","Neutral"],["cool","Cool"]],
                 "default":"neutral","stage":"relight","key":"light","apply":"reload",
                 "enabled_when":[["effect.lighting.relight","eq",true]],
                 "disabled_reason":"Turn on relighting to change this","hint":"segmented"},
                {"id":"effect.lighting.direction","label":"Light direction","type":"int",
                 "min":-90,"max":90,"step":5,"default":0,"unit":"°","stage":"relight",
                 "key":"direction","apply":"live",
                 "enabled_when":[["effect.lighting.relight","eq",true]],
                 "disabled_reason":"Turn on relighting to change this","hint":"slider"},
                {"id":"effect.lighting.brightness","label":"Brightness","type":"int",
                 "min":50,"max":200,"step":5,"default":100,"unit":"%","stage":"relight",
                 "key":"brightness","apply":"live",
                 "enabled_when":[["effect.lighting.relight","eq",true]],
                 "disabled_reason":"Turn on relighting to change this","hint":"slider"}]}
        ]});
        assert_eq!(doc, expected);
        // No setting nor group of the daemon's own background group: the
        // chair setting only reads its mode.
        let d = settings();
        assert!(d.groups.iter().all(|g| g.id != "background"));
        assert!(
            d.groups
                .iter()
                .flat_map(|g| &g.settings)
                .all(|s| !s.id.starts_with("effect.background."))
        );
    }

    fn param<'a>(stage: &'a str, key: &'a str, value: Value<'a>) -> Param<'a> {
        Param { stage, key, value }
    }

    #[test]
    fn the_enhance_method_is_read_from_the_stage_parameter() {
        assert_eq!(enhance_method(&[]).unwrap(), None);
        let p = |v| [param("enhance", "method", v)];
        assert_eq!(
            enhance_method(&p(Value::Str("ai"))).unwrap(),
            Some(EnhanceMethod::Ai)
        );
        assert_eq!(
            enhance_method(&p(Value::Str("fast"))).unwrap(),
            Some(EnhanceMethod::Fast)
        );
        // Another stage's key of the same name is not the method.
        assert_eq!(
            enhance_method(&[param("denoise", "method", Value::Str("fast"))]).unwrap(),
            None
        );
        for bad in [
            Value::Str("turbo"),
            Value::Str("FAST"),
            Value::Bool(true),
            Value::Int(1),
        ] {
            let e = enhance_method(&p(bad)).unwrap_err();
            assert_eq!(e.kind(), obc_module_sdk::ErrorKind::Invalid, "{bad:?}");
            assert!(e.wire_message().contains("ai or fast"), "{bad:?}: {e:?}");
        }
    }

    #[test]
    fn the_chair_reaches_the_backend_and_the_mode_does_not() {
        let params = effect_params(&[
            param("background", "mode", Value::Str("blur")),
            param("background", "chair", Value::Bool(true)),
            param("relight", "light", Value::Str("warm")),
            param(FRAME_GENERATION, "x", Value::Int(1)),
            param(HDR, "x", Value::Int(1)),
        ])
        .unwrap();
        assert_eq!(
            params,
            BTreeMap::from([
                (
                    StageId::BACKGROUND,
                    Params::from([("chair".to_string(), BackendValue::Bool(true))])
                ),
                (
                    StageId::RELIGHT,
                    Params::from([("light".to_string(), BackendValue::Str("warm".into()))])
                ),
            ])
        );
        // The mode alone gives the background stage no parameter.
        let params = effect_params(&[param("background", "mode", Value::Str("replace"))]).unwrap();
        assert!(params.is_empty(), "{params:?}");
        for bad in [Value::Str("yes"), Value::Int(1)] {
            let e = effect_params(&[param("background", "chair", bad)]).unwrap_err();
            assert_eq!(e.kind(), obc_module_sdk::ErrorKind::Invalid, "{bad:?}");
        }
    }

    #[test]
    fn a_method_whose_feature_is_not_installed_is_refused() {
        let only_vsr = tree(&[VSR], &[]);
        let only_upscale = tree(&[UPSCALE], &[]);
        let both = tree(&[VSR, UPSCALE], &[]);
        let ok = |t: &tempfile::TempDir, m| method_installed(Some(t.path()), Some((12, 0)), m);
        assert!(ok(&only_vsr, EnhanceMethod::Ai).is_ok());
        assert!(ok(&only_upscale, EnhanceMethod::Fast).is_ok());
        assert!(ok(&both, EnhanceMethod::Ai).is_ok());
        assert!(ok(&both, EnhanceMethod::Fast).is_ok());
        let e = ok(&only_vsr, EnhanceMethod::Fast).unwrap_err();
        assert_eq!(e.kind(), obc_module_sdk::ErrorKind::Unavailable);
        assert!(e.wire_message().contains("fast"), "{e:?}");
        assert!(e.wire_message().contains("nvvfxupscale"), "{e:?}");
        let e = ok(&only_upscale, EnhanceMethod::Ai).unwrap_err();
        assert!(e.wire_message().contains("nvvfxvideosuperres"), "{e:?}");
        // No root: nothing can be known, the load decides.
        assert!(method_installed(None, None, EnhanceMethod::Fast).is_ok());
    }

    #[test]
    fn without_a_requested_method_the_installed_one_runs() {
        let only_vsr = tree(&[VSR], &[]);
        let only_upscale = tree(&[UPSCALE], &[]);
        let both = tree(&[VSR, UPSCALE], &[]);
        let none = tree(&[DENOISING], &[]);
        let resolve =
            |t: &tempfile::TempDir| resolve_enhance_method(&[], Some(t.path()), Some((12, 0)));
        assert_eq!(resolve(&only_vsr).unwrap(), EnhanceMethod::Ai);
        assert_eq!(resolve(&only_upscale).unwrap(), EnhanceMethod::Fast);
        assert_eq!(resolve(&both).unwrap(), EnhanceMethod::Ai);
        // Neither installed: refused with the preferred method's reason.
        let e = resolve(&none).unwrap_err();
        assert_eq!(e.kind(), obc_module_sdk::ErrorKind::Unavailable);
        assert!(e.wire_message().contains("nvvfxvideosuperres"), "{e:?}");
        // No root: nothing can be known, the default runs.
        assert_eq!(
            resolve_enhance_method(&[], None, None).unwrap(),
            EnhanceMethod::Ai
        );
    }

    #[test]
    fn a_requested_ai_falls_back_to_fast_but_a_requested_fast_is_never_replaced() {
        let only_vsr = tree(&[VSR], &[]);
        let only_upscale = tree(&[UPSCALE], &[]);
        let none = tree(&[DENOISING], &[]);
        let ask = |t: &tempfile::TempDir, m| {
            resolve_enhance_method(
                &[param("enhance", "method", Value::Str(m))],
                Some(t.path()),
                Some((12, 0)),
            )
        };
        assert_eq!(ask(&only_upscale, "ai").unwrap(), EnhanceMethod::Fast);
        let e = ask(&only_vsr, "fast").unwrap_err();
        assert_eq!(e.kind(), obc_module_sdk::ErrorKind::Unavailable);
        assert!(e.wire_message().contains("nvvfxupscale"), "{e:?}");
        let e = ask(&none, "ai").unwrap_err();
        assert!(e.wire_message().contains("nvvfxvideosuperres"), "{e:?}");
        assert_eq!(ask(&only_upscale, "fast").unwrap(), EnhanceMethod::Fast);
        assert_eq!(ask(&only_vsr, "ai").unwrap(), EnhanceMethod::Ai);
        // A malformed method is invalid whatever is installed.
        let e = ask(&only_vsr, "turbo").unwrap_err();
        assert_eq!(e.kind(), obc_module_sdk::ErrorKind::Invalid);
    }

    #[test]
    fn enhance_is_offered_with_either_method_installed() {
        let not = Some(REASON_NOT_INSTALLED.to_string());
        for (features, available) in [
            (&[VSR][..], true),
            (&[UPSCALE][..], true),
            (&[VSR, UPSCALE][..], true),
            (&[DENOISING][..], false),
        ] {
            let t = tree(features, &[]);
            let (r, details) = probe_root(Some(t.path()), Some((12, 0)));
            let enhance = reasons(&r)
                .into_iter()
                .find(|(id, _, _)| id == "enhance")
                .unwrap();
            if available {
                assert_eq!(enhance, ("enhance".into(), true, None), "{features:?}");
                assert!(
                    !details.iter().any(|d| d.starts_with("enhance:")),
                    "{details:?}"
                );
            } else {
                assert_eq!(enhance, ("enhance".into(), false, not.clone()));
                let d = details.iter().find(|d| d.starts_with("enhance:")).unwrap();
                assert!(
                    d.contains("video-super-resolution: ") && d.contains("upscale: "),
                    "{d}"
                );
            }
        }
    }

    /// A fake SDK tree with the feature libraries of `dirs` (directory,
    /// library) and the model files `models`.
    fn tree(features: &[(&str, &str)], models: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (d, lib) in features {
            let l = dir.path().join("features").join(d).join("lib");
            std::fs::create_dir_all(&l).unwrap();
            std::fs::write(l.join(lib), b"not a library").unwrap();
        }
        let m = dir.path().join("lib/models");
        std::fs::create_dir_all(&m).unwrap();
        for name in models {
            std::fs::write(m.join(name), b"").unwrap();
        }
        dir
    }

    const GREEN_SCREEN: (&str, &str) = ("nvvfxgreenscreen", "libnvVFXGreenScreen.so");
    const BLUR: (&str, &str) = ("nvvfxbackgroundblur", "libnvVFXBackgroundBlur.so");
    const DENOISING: (&str, &str) = ("nvvfxdenoising", "libnvVFXDenoising.so");
    const RELIGHTING: (&str, &str) = ("nvvfxrelighting", "libnvVFXRelighting.so");
    const VSR: (&str, &str) = ("nvvfxvideosuperres", "libnvVFXVideoSuperRes.so");
    const UPSCALE: (&str, &str) = ("nvvfxupscale", "libnvVFXUpscale.so");
    const VFG: (&str, &str) = (
        "nvvfxvideoframegeneration",
        "libnvVFXVideoFrameGeneration.so",
    );
    const TRUEHDR: (&str, &str) = ("nvvfxtruehdr", "libnvVFXTrueHDR.so");
    const SM120: &[&str] = &[
        "AIGS_288x512_120_m0.engine.trtpkg",
        "Denoise-agg-120.engine.trtpkg",
        "relight_0thframe_perf_120.engine.trtpkg",
        "relight_nthframe_perf_120.engine.trtpkg",
    ];

    fn reasons(report: &ProbeReport) -> Vec<(String, bool, Option<String>)> {
        report
            .stages
            .iter()
            .map(|s| (s.id.clone(), s.available, s.reason.clone()))
            .collect()
    }

    #[test]
    fn the_probe_reads_the_sdk_tree() {
        let full = tree(
            &[GREEN_SCREEN, BLUR, DENOISING, RELIGHTING, VSR, VFG, TRUEHDR],
            SM120,
        );
        let (r, details) = probe_root(Some(full.path()), Some((12, 0)));
        assert!(r.stages.iter().all(|s| s.available), "{r:?}");
        assert!(details.is_empty());
        let ids: Vec<&str> = r.stages.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "background",
                "denoise",
                "relight",
                "enhance",
                "hdr",
                "frame-generation"
            ]
        );

        // Models for another GPU only: the features with models are not
        // installed for this one; the upscaler has none to miss.
        let (r, details) = probe_root(Some(full.path()), Some((8, 9)));
        let not = Some(REASON_NOT_INSTALLED.to_string());
        assert_eq!(
            reasons(&r),
            [
                ("background".into(), false, not.clone()),
                ("denoise".into(), false, not.clone()),
                ("relight".into(), false, not.clone()),
                ("enhance".into(), true, None),
                ("hdr".into(), true, None),
                ("frame-generation".into(), true, None),
            ]
        );
        assert_eq!(details.len(), 3);
        assert!(details[0].starts_with("background: "), "{details:?}");

        // Relighting without its green screen.
        let part = tree(&[DENOISING, RELIGHTING], SM120);
        let (r, details) = probe_root(Some(part.path()), None);
        assert_eq!(
            reasons(&r),
            [
                ("background".into(), false, not.clone()),
                ("denoise".into(), true, None),
                ("relight".into(), false, not.clone()),
                ("enhance".into(), false, not.clone()),
                ("hdr".into(), false, not.clone()),
                ("frame-generation".into(), false, not),
            ]
        );
        // Without its feature directory, frame generation is not
        // installed, with the reason in the details.
        assert!(
            details
                .iter()
                .any(|d| d.starts_with("frame-generation: the feature library ")),
            "{details:?}"
        );
        assert!(
            details
                .iter()
                .any(|d| d.starts_with("relight: green-screen")),
            "{details:?}"
        );
    }

    #[test]
    fn relighting_is_not_installed_without_the_background_stage() {
        // The green screen and relighting, but not the background effect:
        // relighting has no cut-out to run through.
        let part = tree(&[GREEN_SCREEN, DENOISING, RELIGHTING, VSR, VFG], SM120);
        let (r, details) = probe_root(Some(part.path()), Some((12, 0)));
        let not = Some(REASON_NOT_INSTALLED.to_string());
        assert_eq!(
            reasons(&r),
            [
                ("background".into(), false, not.clone()),
                ("denoise".into(), true, None),
                ("relight".into(), false, not.clone()),
                ("enhance".into(), true, None),
                ("hdr".into(), false, not),
                ("frame-generation".into(), true, None),
            ]
        );
        assert!(
            details
                .iter()
                .any(|d| d.starts_with("relight: needs the background stage")),
            "{details:?}"
        );
    }

    /// The module's `sdk_root` is the root the backend resolves, unless
    /// `MAXINE_VFX_ROOT` overrides it. Nothing is opened.
    #[test]
    fn the_configured_sdk_root_reaches_the_backend() {
        let dir = tempfile::tempdir().unwrap();
        let m = MaxineModule::init(serde_json::json!({"sdk_root": dir.path()})).unwrap();
        assert_eq!(
            obc_backend_maxine::configured_root().as_deref(),
            Some(dir.path())
        );
        let expected = obc_backend_maxine::env_root().unwrap_or_else(|| dir.path().to_path_buf());
        assert_eq!(obc_backend_maxine::effective_root(), Some(expected.clone()));
        assert_eq!(m.effects.root(), Some(expected.as_path()));
        assert_eq!(m.enhance.root(), Some(expected.as_path()));
        // An empty root is no root.
        MaxineModule::init(serde_json::json!({"sdk_root": ""})).unwrap();
        assert_eq!(obc_backend_maxine::configured_root(), None);
    }

    #[test]
    fn without_a_root_nothing_is_known_and_every_stage_is_offered() {
        let (r, details) = probe_root(None, None);
        assert_eq!(r.stages.len(), 6);
        assert!(r.stages.iter().all(|s| s.available));
        assert!(details.is_empty());
    }

    #[test]
    fn the_configuration_takes_the_root_and_the_reserved_host_keys() {
        let c: Config = serde_json::from_value(serde_json::json!({
            "sdk_root": "/opt/sdk", "_host": {"compute_capability": [12, 0], "later": 1}
        }))
        .unwrap();
        assert_eq!(c.sdk_root.as_deref(), Some(Path::new("/opt/sdk")));
        assert_eq!(c.host.compute_capability, Some((12, 0)));
        let c: Config =
            serde_json::from_value(serde_json::json!({"_host": {"compute_capability": null}}))
                .unwrap();
        assert_eq!(c.host.compute_capability, None);
        assert!(serde_json::from_value::<Config>(serde_json::json!({"sdk-root": "/x"})).is_err());
    }

    #[test]
    fn backend_errors_keep_their_kind_and_stage() {
        let e = module_error(BackendError::Stage {
            stage: StageId::RELIGHT,
            code: 3,
            message: "no light".into(),
        });
        assert_eq!(e.failed_stage(), Some("relight"));
        assert_eq!(e.wire_message(), "relight: no light");
        assert_eq!(
            module_error(BackendError::InvalidInput("x".into())).kind(),
            obc_module_sdk::ErrorKind::Invalid
        );
        assert_eq!(
            module_error(BackendError::Unavailable("x".into())).kind(),
            obc_module_sdk::ErrorKind::Unavailable
        );
        assert_eq!(
            module_error(BackendError::Run {
                code: 1,
                message: "x".into()
            })
            .kind(),
            obc_module_sdk::ErrorKind::Failed
        );
    }

    #[test]
    fn the_manifest_declares_the_stages_and_the_crate_version() {
        let text = include_str!("../maxine-vfx.toml");
        let has = |line: &str| text.lines().any(|l| l.trim() == line);
        assert!(has(&format!("id = \"{MODULE_ID}\"")));
        assert!(
            has(&format!("version = \"{}\"", env!("CARGO_PKG_VERSION"))),
            "the version line of crates/obc-module-maxine/maxine-vfx.toml must be the crate's \
             (the workspace version): set it to version = \"{}\"",
            env!("CARGO_PKG_VERSION")
        );
        assert!(has(
            "library = \"/usr/lib/omarchy-broadcast/modules/libobc_module_maxine.so\""
        ));
        for s in STAGES {
            assert!(has(&format!("id = \"{s}\"")), "{s}");
        }
        assert!(has(&format!("id = \"{FRAME_GENERATION}\"")));
        assert!(has(&format!("id = \"{HDR}\"")));
    }

    #[test]
    fn the_hdr_look_is_offered_with_the_truehdr_feature_alone() {
        let not = Some(REASON_NOT_INSTALLED.to_string());
        let hdr_of = |t: &tempfile::TempDir| {
            let (r, details) = probe_root(Some(t.path()), Some((12, 0)));
            let a = reasons(&r)
                .into_iter()
                .find(|(id, _, _)| id == HDR)
                .unwrap();
            (a, details)
        };
        // TrueHDR ships no model: its library is enough, whatever GPU.
        let only = tree(&[TRUEHDR], &[]);
        let (a, details) = hdr_of(&only);
        assert_eq!(a, (HDR.into(), true, None));
        assert!(
            !details.iter().any(|d| d.starts_with("hdr:")),
            "{details:?}"
        );
        let (r, _) = probe_root(Some(only.path()), Some((7, 5)));
        assert!(r.stages.iter().any(|s| s.id == HDR && s.available));
        // Without it: not installed, the library named in the details.
        let without = tree(&[GREEN_SCREEN, BLUR, DENOISING, VFG], SM120);
        let (a, details) = hdr_of(&without);
        assert_eq!(a, (HDR.into(), false, not));
        let d = details.iter().find(|d| d.starts_with("hdr: ")).unwrap();
        assert!(
            d.contains("nvvfxtruehdr") && d.contains("libnvVFXTrueHDR.so"),
            "{d}"
        );
    }
}
