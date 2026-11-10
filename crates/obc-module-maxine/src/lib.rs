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
//! | `enhance` | pre-matte (position 0) | video super resolution, alone in its chain (the ingest's) |
//! | `denoise` | pre-matte | noise removal |
//! | `background` | matte | the cut-out, and the blur or the replacement of the background |
//! | `relight` | post-matte, needs the matte | relighting of the person |
//!
//! A chain of `background`, `denoise` and `relight` is the Maxine effect
//! backend ([`obc_backend_maxine::MaxineBackend`]): the `background` stage's
//! load-time parameter `mode` (`blur` or `replace`) is the effect, its live
//! parameter `blur_strength` (0 to 1) the blur strength, and the chain's
//! background frame the replacement image; without `background` the effect
//! is `none`. A load on a loaded chain is a switch on the same backend, so
//! models are reused as before. A chain of `enhance` alone is the Maxine
//! upscaler ([`obc_backend_maxine::vsr::MaxineVsr`]): NV12 or I420 in, NV12
//! an integer factor (2 to 4) larger out. Frames are CUDA frames only.
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
//! log). Relighting needs the cut-out of the `background` stage, so it is
//! unavailable whenever `background` is. Without an SDK root to read,
//! nothing can be known, and every stage is reported available.
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
    BackendError, BackendFactory, ColourMatrix as BackendMatrix, ColourSpec, DeviceNv12,
    EffectKind, FrameParams, GpuContext, HostBgr, InferenceBackend, LoadRequest, Params, StageId,
    StageList, StageTimings, Value as BackendValue,
};
use obc_backend_maxine::vsr::{MaxineVsr, MaxineVsrFactory};
use obc_backend_maxine::{
    MaxineBackend, MaxineFactory, RELIGHT_BRIGHTNESS, RELIGHT_DIRECTION, RELIGHT_LIGHT,
    RelightLight, STAGES, feature_availability, stage_availability_of,
};
use obc_module_sdk::{
    Apply, ChainRequest, ChromaSiting, ColourMatrix, Error, Frame, FrameKind, Group, Instance,
    LoadReport, Module, Op, Param, PixelFormat, ProbeReport, Role, Setting, SettingsDescription,
    Value, log, serde_json,
};
use serde::Deserialize;

/// The module id.
pub const MODULE_ID: &str = "br.com.dias.maxine-vfx";

/// The reason of a stage whose SDK features are not installed.
pub const REASON_NOT_INSTALLED: &str = "Not installed";
/// Why a relighting setting is greyed while relighting is off.
pub const REASON_TURN_ON_RELIGHT: &str = "Turn on relighting to change this";

/// The `background` stage's load-time parameter with the effect.
pub const BACKGROUND_MODE: &str = "mode";
/// The `background` stage's live parameter with the blur strength.
pub const BACKGROUND_BLUR_STRENGTH: &str = "blur_strength";

/// The video group's settings.
pub const DENOISE: &str = "effect.video.denoise";
/// See [`DENOISE`].
pub const ENHANCE: &str = "effect.video.enhance";
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

/// The settings of the module: the video group (noise removal and
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
    SettingsDescription::new()
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
/// (`None`: any): every stage, with [`REASON_NOT_INSTALLED`] for those not
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
        return (report, details);
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
    (report, details)
}

/// The module object.
pub struct MaxineModule {
    effects: MaxineFactory,
    enhance: MaxineVsrFactory,
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
        match effects.root() {
            Some(r) => log::debug!("the Maxine SDK root is {}", r.display()),
            None => log::debug!("no Maxine SDK root: the library search path is used"),
        }
        Ok(MaxineModule {
            effects,
            enhance,
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
    /// `relight`, the upscaler's libraries for `enhance`. No CUDA context,
    /// no model.
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
        Ok(())
    }

    fn create_instance(&self) -> Result<MaxineInstance, Error> {
        Ok(MaxineInstance {
            effects: self.effects.clone(),
            enhance: self.enhance.clone(),
            chain: Chain::Empty,
            gpu: None,
            blur_strength: FrameParams::default().blur_strength,
            live: BTreeMap::new(),
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
}

/// One chain of the module's stages.
pub struct MaxineInstance {
    effects: MaxineFactory,
    enhance: MaxineVsrFactory,
    chain: Chain,
    /// The context and stream of the last load.
    gpu: Option<GpuContext>,
    /// The last blur strength a frame gave.
    blur_strength: f32,
    /// The last live parameters frames gave, by stage.
    live: BTreeMap<StageId, Params>,
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

impl MaxineInstance {
    fn load_effect(
        &mut self,
        req: &ChainRequest<'_>,
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
        let mut stages = Vec::new();
        for s in req.stages() {
            match *s {
                "background" => {}
                "denoise" | "relight" => {
                    stages.push(StageId::new(*s).map_err(|e| Error::invalid(e.to_string()))?)
                }
                other => {
                    return Err(Error::invalid(format!(
                        "the Maxine module has no stage {other:?} for the effect chain"
                    )));
                }
            }
        }
        let stages = StageList::new(stages).map_err(|e| Error::invalid(e.to_string()))?;
        let mut params: BTreeMap<StageId, Params> = BTreeMap::new();
        for p in req.params() {
            if p.stage == StageId::BACKGROUND.as_str() {
                continue;
            }
            let stage = StageId::new(p.stage).map_err(|e| Error::invalid(e.to_string()))?;
            params
                .entry(stage)
                .or_default()
                .insert(p.key.to_string(), value(&p.value));
        }
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
        };
        if !matches!(self.chain, Chain::Enhance(_)) {
            self.unload_chain();
            let vsr = self.enhance.create_vsr().map_err(module_error)?;
            self.chain = Chain::Enhance(Box::new(vsr));
        }
        let Chain::Enhance(vsr) = &mut self.chain else {
            unreachable!("set above");
        };
        let report = vsr.load(&gpu, &enhance).map_err(module_error)?;
        Ok(LoadReport {
            load_ms: report.load_ms as f32,
            warmup_ms: report.warmup_ms.map(|w| w as f32),
            reused_model: report.reused_model,
        })
    }

    fn unload_chain(&mut self) {
        match &mut self.chain {
            Chain::Empty => {}
            Chain::Effect(b) => b.unload(),
            Chain::Enhance(v) => v.unload(),
        }
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
        let result = if req.has_stage(StageId::ENHANCE.as_str()) {
            if req.stages().len() != 1 {
                return Err(Error::unsupported(
                    "the enhance stage runs alone in its chain",
                ));
            }
            self.load_enhance(req, gpu)
        } else {
            self.load_effect(req, gpu)
        };
        if result.is_ok() {
            self.gpu = Some(gpu);
            // The live parameters of the previous chain do not carry over:
            // each frame gives those in force.
            self.live.clear();
            self.blur_strength = FrameParams::default().blur_strength;
        }
        result
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
        match &mut self.chain {
            Chain::Empty => Err(Error::state("nothing is loaded")),
            Chain::Effect(backend) => {
                let params = FrameParams {
                    blur_strength: self.blur_strength,
                    live: self.live.clone(),
                };
                backend
                    .process(&gpu, &params, &device_nv12(input)?, &device_nv12(output)?)
                    .map_err(module_error)
            }
            Chain::Enhance(vsr) => vsr
                .process(&gpu, &device_yuv420(input)?, &device_nv12(output)?)
                .map_err(module_error),
        }
    }

    fn reset_temporal(&mut self) -> Result<Vec<String>, Error> {
        let (Chain::Effect(backend), Some(gpu)) = (&mut self.chain, &self.gpu) else {
            return Ok(Vec::new());
        };
        let dropped = backend.reset_temporal(gpu).map_err(module_error)?;
        Ok(dropped.iter().map(|s| s.to_string()).collect())
    }

    fn unload(&mut self) {
        self.unload_chain();
        self.gpu = None;
        self.live.clear();
        self.blur_strength = FrameParams::default().blur_strength;
    }

    fn timings(&self) -> Vec<(String, f32)> {
        match &self.chain {
            Chain::Empty => Vec::new(),
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
    fn the_settings_are_the_daemons_video_and_lighting_groups() {
        let doc: serde_json::Value = serde_json::from_str(&settings().to_json()).unwrap();
        let expected = serde_json::json!({"groups":[
            {"id":"video","label":"Video","settings":[
                {"id":"effect.video.denoise","label":"Noise removal","type":"bool","default":false,
                 "stage":"denoise","role":"enable","hint":"switch"},
                {"id":"effect.video.enhance","label":"Enhance low-resolution camera","type":"bool",
                 "default":false,"stage":"enhance","role":"enable","hint":"switch"}]},
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
        // No setting of the daemon's own background group.
        assert!(!settings().to_json().contains("effect.background"));
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
        let full = tree(&[GREEN_SCREEN, BLUR, DENOISING, RELIGHTING, VSR], SM120);
        let (r, details) = probe_root(Some(full.path()), Some((12, 0)));
        assert!(r.stages.iter().all(|s| s.available), "{r:?}");
        assert!(details.is_empty());
        let ids: Vec<&str> = r.stages.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["background", "denoise", "relight", "enhance"]);

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
                ("enhance".into(), false, not),
            ]
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
        let part = tree(&[GREEN_SCREEN, DENOISING, RELIGHTING, VSR], SM120);
        let (r, details) = probe_root(Some(part.path()), Some((12, 0)));
        let not = Some(REASON_NOT_INSTALLED.to_string());
        assert_eq!(
            reasons(&r),
            [
                ("background".into(), false, not.clone()),
                ("denoise".into(), true, None),
                ("relight".into(), false, not),
                ("enhance".into(), true, None),
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
        assert_eq!(r.stages.len(), 4);
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
    }
}
