//! The in-process seams of `obc-backend` over a module's instances, so that
//! the pipeline elements run module stages exactly as they run built-in
//! backends:
//!
//! * [`ModuleBackendFactory`]: [`BackendFactory`] / [`InferenceBackend`],
//!   for the effect element. The effect's [`EffectKind`] becomes the
//!   `background` stage of the chain, with the load-time parameter
//!   [`BACKGROUND_MODE`] (`blur` or `replace`) and, for `replace`, the
//!   background image as a host BGR frame; the frame's blur strength
//!   becomes the live parameter [`BACKGROUND_BLUR_STRENGTH`] (a float, 0 to
//!   1). The chain is the request's pre-matte stages, `background`, then
//!   its other stages, by the slots of the manifest.
//! * [`ModuleEnhancerFactory`]: [`EnhancerFactory`] / [`Enhancer`], for the
//!   ingest's `enhance` stage: a 4:2:0 CUDA frame (NV12 or I420) in, an NV12
//!   frame `scale` times as large out.
//! * [`ModuleChainFactory`]: [`StageChainFactory`] / [`StageChain`], for a
//!   chain of a module's other stages, on CUDA or host frames as the
//!   manifest declares, in place.
//!
//! Failures keep the seams' meaning: a stage alone failing (`OBC_ERR_STAGE`)
//! is [`BackendError::Stage`]; an invalid argument or a call in the wrong
//! state is [`BackendError::InvalidInput`]; what the module cannot find is
//! [`BackendError::Unavailable`]; anything else fails the load
//! ([`BackendError::Load`]) or the frame ([`BackendError::Run`]), with the
//! ABI status as the code and the module's reason as the message. A failed
//! switch keeps the previous chain loaded, a failed full load leaves the
//! instance unloaded, as the seams say (the ABI's switch rule); a failure
//! of a matte stage is never a stage failure: it fails the whole chain and
//! unloads it ([`crate::module`]).
//!
//! The factories' `probe` asks the module's probe and then its deeper
//! `check` of the stages the factory runs, which may open the module's own
//! libraries but never touches the GPU: a missing runtime is told before
//! the camera opens.
//!
//! The caller keeps the seams' CUDA rule: the context of the frames is
//! current around every call. These adapters never touch it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use obc_backend::enhance::{
    ChromaSiting, DeviceYuv420, EnhanceRequest, EnhanceTimings, Enhancer, EnhancerFactory,
    YuvLayout,
};
use obc_backend::{
    BackendError, BackendFactory, ChainFrame, ChainRequest, ColourMatrix, DeviceNv12, EffectKind,
    FrameKind, FrameParams, GpuContext, HostNv12, InferenceBackend, LoadReport, LoadRequest,
    Params, StageChain, StageChainFactory, StageId, StageList, StageTimings, Value,
};
use obc_module_abi::{
    OBC_CHROMA_SITING_COSITED, OBC_CHROMA_SITING_JPEG, OBC_CHROMA_SITING_MPEG2, OBC_FORMAT_I420,
    OBC_FORMAT_NV12, OBC_FRAME_CUDA, OBC_FRAME_FLAG_REPEAT, OBC_FRAME_HOST, OBC_MATRIX_BT601,
    OBC_MATRIX_BT709, obc_cuda, obc_frame, obc_matrix, obc_plane,
};

use crate::manifest::{Frames, Slot};
use crate::module::{ChainSpec, Instance, LoadOutcome, Module, ModuleError, Status};
use crate::settings::{SettingType, SettingsDescription};

/// The load-time parameter of the `background` stage that says which
/// effect it composites: `blur` or `replace` (a string).
pub const BACKGROUND_MODE: &str = "mode";
/// The live parameter of the `background` stage with the blur strength, a
/// float from 0 to 1.
pub const BACKGROUND_BLUR_STRENGTH: &str = "blur_strength";

/// A `&'static str` for a name that lives as long as the process: the seams
/// want one, and module ids are few.
fn intern(name: &str) -> &'static str {
    static NAMES: Mutex<BTreeSet<&'static str>> = Mutex::new(BTreeSet::new());
    let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(n) = names.get(name) {
        return n;
    }
    let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
    names.insert(leaked);
    leaked
}

/// The phase of a call, for its error.
#[derive(Clone, Copy)]
enum Phase {
    Load,
    Run,
}

fn backend_error(e: ModuleError, phase: Phase) -> BackendError {
    match (e.status, e.stage) {
        (Status::Stage, Some(stage)) => BackendError::Stage {
            stage,
            code: e.code,
            message: e.message,
        },
        (Status::Invalid | Status::State, _) => BackendError::InvalidInput(e.message),
        (Status::Unavailable, _) => BackendError::Unavailable(e.message),
        _ => match phase {
            Phase::Load => BackendError::Load {
                code: e.code,
                message: e.message,
            },
            Phase::Run => BackendError::Run {
                code: e.code,
                message: e.message,
            },
        },
    }
}

fn load_report(o: LoadOutcome) -> LoadReport {
    LoadReport {
        load_ms: f64::from(o.load_ms),
        warmup_ms: o.warmup_ms.map(f64::from),
        reused_model: o.reused_model,
    }
}

fn matrix(m: ColourMatrix) -> obc_matrix {
    match m {
        ColourMatrix::Bt601 => OBC_MATRIX_BT601,
        ColourMatrix::Bt709 => OBC_MATRIX_BT709,
    }
}

fn cuda(gpu: &GpuContext) -> obc_cuda {
    obc_cuda {
        context: gpu.context,
        stream: gpu.stream,
    }
}

fn check_context(gpu: &GpuContext) -> Result<(), BackendError> {
    if gpu.context.is_null() {
        return Err(BackendError::InvalidInput(
            "a CUDA module chain needs the frames' CUDA context".into(),
        ));
    }
    Ok(())
}

fn plane(addr: u64, pitch: usize) -> obc_plane {
    obc_plane {
        ptr_or_fd: addr,
        pitch: pitch as u64,
        offset: 0,
    }
}

/// A CUDA NV12 frame over `f`.
fn cuda_nv12(f: &DeviceNv12, gpu: &GpuContext) -> obc_frame {
    let mut o = obc_frame::empty();
    o.kind = OBC_FRAME_CUDA;
    o.format = OBC_FORMAT_NV12;
    o.width = f.width;
    o.height = f.height;
    o.n_planes = 2;
    o.planes[0] = plane(f.y, f.y_pitch);
    o.planes[1] = plane(f.uv, f.uv_pitch);
    o.cuda = cuda(gpu);
    o
}

/// A CUDA NV12 or I420 frame over `f`.
fn cuda_yuv420(f: &DeviceYuv420, gpu: &GpuContext) -> obc_frame {
    let mut o = obc_frame::empty();
    o.kind = OBC_FRAME_CUDA;
    o.width = f.width;
    o.height = f.height;
    o.planes[0] = plane(f.y, f.y_pitch);
    o.planes[1] = plane(f.u, f.u_pitch);
    match f.layout {
        YuvLayout::Nv12 => {
            o.format = OBC_FORMAT_NV12;
            o.n_planes = 2;
        }
        YuvLayout::I420 => {
            o.format = OBC_FORMAT_I420;
            o.n_planes = 3;
            o.planes[2] = plane(f.v, f.v_pitch);
        }
    }
    o.cuda = cuda(gpu);
    o
}

/// A host NV12 frame over `f`.
fn host_nv12(f: &HostNv12) -> obc_frame {
    let mut o = obc_frame::empty();
    o.kind = OBC_FRAME_HOST;
    o.format = OBC_FORMAT_NV12;
    o.width = f.width;
    o.height = f.height;
    o.n_planes = 2;
    o.planes[0] = plane(f.y as u64, f.y_pitch);
    o.planes[1] = plane(f.uv as u64, f.uv_pitch);
    o
}

/// The parameters of the stages of `chain`, flattened.
fn flatten<'a>(
    params: &'a BTreeMap<StageId, Params>,
    chain: &'a [StageId],
) -> impl Iterator<Item = (&'a str, &'a str, &'a Value)> {
    params
        .iter()
        .filter(|(stage, _)| chain.contains(stage))
        .flat_map(|(stage, p)| p.iter().map(move |(k, v)| (stage.as_str(), k.as_str(), v)))
}

/// Checks `value` against the setting the module described for it.
fn check_value(
    stage: &StageId,
    key: &str,
    kind: &SettingType,
    value: &Value,
) -> Result<(), String> {
    let bad = |want: &str| {
        Err(format!(
            "the {stage} parameter {key} must be {want}, not {value} ({})",
            value.kind()
        ))
    };
    match kind {
        SettingType::Bool { .. } => match value {
            Value::Bool(_) => Ok(()),
            _ => bad("a boolean"),
        },
        SettingType::Int { min, max, .. } => {
            let n = match value {
                Value::Int(i) => *i as f64,
                Value::Float(f) => *f,
                _ => return bad("a number"),
            };
            if n.is_finite() && (*min as f64..=*max as f64).contains(&n) {
                Ok(())
            } else {
                Err(format!(
                    "the {stage} parameter {key} is {value}, outside {min} to {max}"
                ))
            }
        }
        SettingType::Enum { options, .. } => match value {
            Value::Str(s) if options.iter().any(|(v, _)| v == s) => Ok(()),
            Value::Str(s) => {
                let names: Vec<&str> = options.iter().map(|(v, _)| v.as_str()).collect();
                Err(format!(
                    "the {stage} parameter {key} is {s:?}, not one of {}",
                    names.join(", ")
                ))
            }
            _ => bad("a string"),
        },
        SettingType::Action => Ok(()),
    }
}

/// Checks parameters against a module's settings description: a key the
/// module described as a parameter of `stage` must have a value of its type
/// and range; other keys are accepted.
pub fn check_params(
    settings: &SettingsDescription,
    stage: &StageId,
    params: &Params,
    live: &Params,
) -> Result<(), String> {
    for (key, value) in params.iter().chain(live) {
        if let Some(s) = settings.param(stage.as_str(), key) {
            check_value(stage, key, &s.kind, value)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The effect
// ---------------------------------------------------------------------------

/// The module's probe, then its check of `stages`: `Unavailable` with the
/// reason of the first that fails.
fn probe_and_check(module: &Module, stages: &[StageId]) -> Result<(), BackendError> {
    module
        .probe()
        .map_err(|e| BackendError::Unavailable(e.message))?;
    module
        .check(stages)
        .map_err(|e| BackendError::Unavailable(e.message))
}

/// A [`BackendFactory`] over a module that provides the `background` stage
/// (and the stages that run with it in the effect element).
pub struct ModuleBackendFactory {
    module: Module,
    name: &'static str,
    settings: Option<SettingsDescription>,
}

impl ModuleBackendFactory {
    /// The factory of `module`. Checks parameters
    /// ([`BackendFactory::check_params`]) against the settings description
    /// read when the module was opened ([`Module::cached_settings`]): the
    /// factory never calls the module for it, so no module code runs
    /// outside the crash ledger's marker. Without one every parameter is
    /// accepted.
    pub fn new(module: Module) -> ModuleBackendFactory {
        let settings = module.cached_settings();
        ModuleBackendFactory {
            name: intern(module.id()),
            module,
            settings,
        }
    }

    /// The module.
    pub fn module(&self) -> &Module {
        &self.module
    }
}

impl BackendFactory for ModuleBackendFactory {
    /// The module id.
    fn name(&self) -> &'static str {
        self.name
    }

    /// The module's probe answers (filesystem only), and its deeper check
    /// passes for the cut-out (`background`); which stages are available
    /// is the registry's to say.
    fn probe(&self) -> Result<(), BackendError> {
        probe_and_check(&self.module, &[StageId::BACKGROUND])
    }

    fn create(&self) -> Result<Box<dyn InferenceBackend>, BackendError> {
        let instance = self
            .module
            .create_instance()
            .map_err(|e| BackendError::Unavailable(e.message))?;
        let slots = self
            .module
            .manifest()
            .stages
            .iter()
            .map(|s| (s.id.clone(), s.slot))
            .collect();
        Ok(Box::new(ModuleBackend {
            instance,
            name: self.name,
            slots,
            background: false,
        }))
    }

    fn check_params(&self, stage: &StageId, params: &Params, live: &Params) -> Result<(), String> {
        match &self.settings {
            Some(s) => check_params(s, stage, params, live),
            None => Ok(()),
        }
    }
}

/// The ordered chain of an effect request: the request's stages whose slot
/// comes before the matte (or that the manifest does not declare), then
/// `background` (for `blur` and `replace`), then the others, each part in
/// the request's order.
fn effect_chain(req: &LoadRequest, slots: &BTreeMap<StageId, Slot>) -> Vec<StageId> {
    let (pre, post): (Vec<StageId>, Vec<StageId>) = req
        .stages
        .iter()
        .filter(|s| **s != StageId::BACKGROUND)
        .cloned()
        .partition(|s| slots.get(s).is_none_or(|slot| *slot < Slot::Matte));
    let mut chain = pre;
    if req.effect != EffectKind::None {
        chain.push(StageId::BACKGROUND);
    }
    chain.extend(post);
    chain
}

/// An [`InferenceBackend`] over a module instance.
pub struct ModuleBackend {
    instance: Instance,
    name: &'static str,
    slots: BTreeMap<StageId, Slot>,
    /// Whether the loaded chain has the `background` stage.
    background: bool,
}

impl InferenceBackend for ModuleBackend {
    fn name(&self) -> &'static str {
        self.name
    }

    fn load(&mut self, gpu: &GpuContext, req: &LoadRequest) -> Result<LoadReport, BackendError> {
        check_context(gpu)?;
        let stages = effect_chain(req, &self.slots);
        let mut params: Vec<(StageId, String, Value)> = flatten(&req.params, &stages)
            .map(|(s, k, v)| {
                (
                    StageId::new(s).expect("a stage id"),
                    k.to_string(),
                    v.clone(),
                )
            })
            .collect();
        if req.effect != EffectKind::None {
            params.push((
                StageId::BACKGROUND,
                BACKGROUND_MODE.into(),
                Value::Str(req.effect.as_str().into()),
            ));
        }
        let spec = ChainSpec {
            stages,
            kind: OBC_FRAME_CUDA,
            input_format: OBC_FORMAT_NV12,
            input_size: (req.width, req.height),
            output_format: OBC_FORMAT_NV12,
            output_size: (req.width, req.height),
            matrix: matrix(req.colour.matrix),
            chroma_siting: OBC_CHROMA_SITING_MPEG2,
            full_range: req.colour.full_range,
            cuda: cuda(gpu),
            params,
            background: (req.effect == EffectKind::Replace)
                .then(|| req.background.clone())
                .flatten(),
        };
        let result = self.instance.load(&spec);
        self.background = self.instance.chain().contains(&StageId::BACKGROUND);
        result
            .map(load_report)
            .map_err(|e| backend_error(e, Phase::Load))
    }

    fn process(
        &mut self,
        gpu: &GpuContext,
        params: &FrameParams,
        src: &DeviceNv12,
        dst: &DeviceNv12,
    ) -> Result<(), BackendError> {
        check_context(gpu)?;
        src.validate()?;
        dst.validate()?;
        let chain = self.instance.chain().to_vec();
        let strength = Value::Float(f64::from(params.blur_strength.clamp(0.0, 1.0)));
        let mut live: Vec<(&str, &str, &Value)> = Vec::new();
        if self.background {
            live.push((
                StageId::BACKGROUND.as_str(),
                BACKGROUND_BLUR_STRENGTH,
                &strength,
            ));
        }
        live.extend(flatten(&params.live, &chain));
        let (input, output) = (cuda_nv12(src, gpu), cuda_nv12(dst, gpu));
        // SAFETY: the seam's contract: the frames are device memory of the
        // current context, valid while the work is enqueued; the module
        // reads `src` and writes `dst` only.
        let result = unsafe { self.instance.process(&input, &output, &live) };
        result.map_err(|e| backend_error(e, Phase::Run))
    }

    fn unload(&mut self) {
        self.instance.unload();
        self.background = false;
    }

    /// Whether the instance holds a chain: not after a failed full load, a
    /// failed matte stage or a panic, which unload it.
    fn is_loaded(&self) -> bool {
        self.instance.is_loaded()
    }

    fn timings(&self) -> Option<StageTimings> {
        let steps = self.instance.timings();
        if steps.is_empty() {
            return None;
        }
        let mut t = StageTimings::new();
        for (step, ms) in steps {
            t.push(step, ms);
        }
        Some(t)
    }

    fn reset_temporal(&mut self, gpu: &GpuContext) -> Result<StageList, BackendError> {
        let _ = gpu;
        if !self.instance.is_loaded() {
            return Ok(StageList::default());
        }
        let dropped = self
            .instance
            .reset_temporal()
            .map_err(|e| backend_error(e, Phase::Run))?;
        if dropped.contains(StageId::BACKGROUND.as_str()) {
            self.background = false;
        }
        Ok(dropped)
    }
}

// ---------------------------------------------------------------------------
// The enhance stage
// ---------------------------------------------------------------------------

/// An [`EnhancerFactory`] over a module that provides the `enhance` stage.
pub struct ModuleEnhancerFactory {
    module: Module,
    name: &'static str,
}

impl ModuleEnhancerFactory {
    /// The factory of `module`.
    pub fn new(module: Module) -> ModuleEnhancerFactory {
        ModuleEnhancerFactory {
            name: intern(module.id()),
            module,
        }
    }

    /// The module.
    pub fn module(&self) -> &Module {
        &self.module
    }
}

impl EnhancerFactory for ModuleEnhancerFactory {
    /// The module id.
    fn name(&self) -> &'static str {
        self.name
    }

    /// The module's probe answers, and its deeper check passes for
    /// `enhance`.
    fn probe(&self) -> Result<(), BackendError> {
        probe_and_check(&self.module, &[StageId::ENHANCE])
    }

    fn create(&self) -> Result<Box<dyn Enhancer>, BackendError> {
        let instance = self
            .module
            .create_instance()
            .map_err(|e| BackendError::Unavailable(e.message))?;
        Ok(Box::new(ModuleEnhancer {
            instance,
            name: self.name,
        }))
    }
}

/// An [`Enhancer`] over a module instance loaded with the `enhance` stage.
pub struct ModuleEnhancer {
    instance: Instance,
    name: &'static str,
}

fn chroma_siting(c: ChromaSiting) -> u32 {
    match c {
        ChromaSiting::Cosited => OBC_CHROMA_SITING_MPEG2,
        ChromaSiting::Interstitial => OBC_CHROMA_SITING_JPEG,
        ChromaSiting::TopLeft => OBC_CHROMA_SITING_COSITED,
    }
}

impl Enhancer for ModuleEnhancer {
    fn name(&self) -> &'static str {
        self.name
    }

    fn load(&mut self, gpu: &GpuContext, req: &EnhanceRequest) -> Result<LoadReport, BackendError> {
        check_context(gpu)?;
        req.validate()?;
        let spec = ChainSpec {
            stages: vec![StageId::ENHANCE],
            kind: OBC_FRAME_CUDA,
            input_format: match req.layout {
                YuvLayout::Nv12 => OBC_FORMAT_NV12,
                YuvLayout::I420 => OBC_FORMAT_I420,
            },
            input_size: (req.width, req.height),
            output_format: OBC_FORMAT_NV12,
            output_size: req.output_size(),
            matrix: matrix(req.colour.matrix),
            chroma_siting: chroma_siting(req.chroma),
            full_range: req.colour.full_range,
            cuda: cuda(gpu),
            params: req
                .params
                .iter()
                .map(|(k, v)| (StageId::ENHANCE, k.clone(), v.clone()))
                .collect(),
            background: None,
        };
        self.instance
            .load(&spec)
            .map(load_report)
            .map_err(|e| backend_error(e, Phase::Load))
    }

    fn process(
        &mut self,
        gpu: &GpuContext,
        src: &DeviceYuv420,
        dst: &DeviceNv12,
    ) -> Result<(), BackendError> {
        check_context(gpu)?;
        src.validate()?;
        dst.validate()?;
        let (input, output) = (cuda_yuv420(src, gpu), cuda_nv12(dst, gpu));
        // SAFETY: the seam's contract, as for the effect.
        let result = unsafe { self.instance.process(&input, &output, &[]) };
        result.map_err(|e| backend_error(e, Phase::Run))
    }

    fn unload(&mut self) {
        self.instance.unload();
    }

    /// The module's `convert-in`, `enhance` (or `upscale`) and
    /// `convert-out` steps.
    fn timings(&self) -> Option<EnhanceTimings> {
        let steps = self.instance.timings();
        if steps.is_empty() {
            return None;
        }
        let of = |names: &[&str]| {
            steps
                .iter()
                .find(|(s, _)| names.contains(&s.as_str()))
                .map_or(0.0, |(_, ms)| *ms)
        };
        Some(EnhanceTimings {
            convert_in_ms: of(&[StageTimings::CONVERT_IN]),
            upscale_ms: of(&[StageId::ENHANCE.as_str(), "upscale"]),
            convert_out_ms: of(&[StageTimings::CONVERT_OUT]),
        })
    }
}

// ---------------------------------------------------------------------------
// Other chains
// ---------------------------------------------------------------------------

/// A [`StageChainFactory`] over a module, for a chain of some of its
/// stages.
pub struct ModuleChainFactory {
    module: Module,
    kinds: Vec<FrameKind>,
}

impl ModuleChainFactory {
    /// The factory of chains of `stages` of `module`: its frame kinds are
    /// those every one of the stages takes (CUDA and host; a DMA-BUF kind is
    /// not offered yet), in the first stage's order of preference. Refused
    /// for no stage, a stage the manifest does not declare, or stages with
    /// no kind in common.
    pub fn new(module: Module, stages: &[StageId]) -> Result<ModuleChainFactory, String> {
        let manifest = module.manifest();
        let mut decls = Vec::with_capacity(stages.len());
        for s in stages {
            decls.push(manifest.stage(s.as_str()).ok_or_else(|| {
                format!("the module {} does not provide the stage {s}", manifest.id)
            })?);
        }
        let Some(first) = decls.first() else {
            return Err("a stage chain needs at least one stage".into());
        };
        let kinds: Vec<FrameKind> = first
            .frames
            .iter()
            .filter(|f| decls.iter().all(|d| d.frames.contains(f)))
            .filter_map(|f| match f {
                Frames::Cuda => Some(FrameKind::Cuda),
                Frames::Host => Some(FrameKind::Host),
                Frames::Dmabuf => None,
            })
            .collect();
        if kinds.is_empty() {
            let names: Vec<&str> = stages.iter().map(StageId::as_str).collect();
            return Err(format!(
                "the stages {} of the module {} take no frame kind in common",
                names.join(", "),
                manifest.id
            ));
        }
        Ok(ModuleChainFactory { module, kinds })
    }

    /// The module.
    pub fn module(&self) -> &Module {
        &self.module
    }
}

impl StageChainFactory for ModuleChainFactory {
    /// The module id.
    fn name(&self) -> &str {
        self.module.id()
    }

    fn kinds(&self) -> &[FrameKind] {
        &self.kinds
    }

    fn create(&self) -> Result<Box<dyn StageChain>, BackendError> {
        let instance = self
            .module
            .create_instance()
            .map_err(|e| BackendError::Unavailable(e.message))?;
        Ok(Box::new(ModuleChain {
            instance,
            kinds: self.kinds.clone(),
            kind: None,
        }))
    }
}

/// A [`StageChain`] over a module instance.
pub struct ModuleChain {
    instance: Instance,
    kinds: Vec<FrameKind>,
    /// The frame kind of the loaded chain.
    kind: Option<FrameKind>,
}

impl StageChain for ModuleChain {
    fn load(&mut self, req: &ChainRequest) -> Result<LoadReport, BackendError> {
        req.validate()?;
        if !self.kinds.contains(&req.kind) {
            return Err(BackendError::InvalidInput(format!(
                "the module {} takes no {} frames for these stages",
                self.instance.module_id(),
                req.kind
            )));
        }
        let stages: Vec<StageId> = req.stages.iter().cloned().collect();
        let params = flatten(&req.params, &stages)
            .map(|(s, k, v)| {
                (
                    StageId::new(s).expect("a stage id"),
                    k.to_string(),
                    v.clone(),
                )
            })
            .collect();
        let (kind, cuda_handles) = match (req.kind, &req.gpu) {
            (FrameKind::Cuda, Some(gpu)) => (OBC_FRAME_CUDA, cuda(gpu)),
            _ => (OBC_FRAME_HOST, obc_cuda::null()),
        };
        let spec = ChainSpec {
            stages,
            kind,
            input_format: OBC_FORMAT_NV12,
            input_size: (req.width, req.height),
            output_format: OBC_FORMAT_NV12,
            output_size: (req.width, req.height),
            matrix: matrix(req.colour.matrix),
            chroma_siting: OBC_CHROMA_SITING_MPEG2,
            full_range: req.colour.full_range,
            cuda: cuda_handles,
            params,
            background: None,
        };
        match self.instance.load(&spec) {
            Ok(o) => {
                self.kind = Some(req.kind);
                Ok(load_report(o))
            }
            Err(e) => {
                if !self.instance.is_loaded() {
                    self.kind = None;
                }
                Err(backend_error(e, Phase::Load))
            }
        }
    }

    fn process(
        &mut self,
        frame: ChainFrame<'_>,
        live: &BTreeMap<StageId, Params>,
    ) -> Result<(), BackendError> {
        self.run(frame, live, 0)
    }

    /// The frame with `OBC_FRAME_FLAG_REPEAT`, which the module sees from
    /// ABI 1.1 on (the host clears it for a module of ABI 1.0).
    fn process_repeat(
        &mut self,
        frame: ChainFrame<'_>,
        live: &BTreeMap<StageId, Params>,
    ) -> Result<(), BackendError> {
        self.run(frame, live, OBC_FRAME_FLAG_REPEAT)
    }

    fn reset_temporal(&mut self) -> Result<StageList, BackendError> {
        if !self.instance.is_loaded() {
            return Ok(StageList::default());
        }
        self.instance
            .reset_temporal()
            .map_err(|e| backend_error(e, Phase::Run))
    }

    fn unload(&mut self) {
        self.instance.unload();
        self.kind = None;
    }
}

impl ModuleChain {
    /// Runs one frame, in place, with the `OBC_FRAME_FLAG_*` bits `flags`.
    fn run(
        &mut self,
        frame: ChainFrame<'_>,
        live: &BTreeMap<StageId, Params>,
        flags: u32,
    ) -> Result<(), BackendError> {
        if self.kind != Some(frame.kind()) {
            return Err(BackendError::InvalidInput(format!(
                "a {} frame for a chain loaded for {}",
                frame.kind(),
                self.kind.map_or("nothing", FrameKind::as_str)
            )));
        }
        let chain = self.instance.chain().to_vec();
        let live: Vec<(&str, &str, &Value)> = flatten(live, &chain).collect();
        let mut raw = match &frame {
            ChainFrame::Cuda { gpu, frame } => {
                check_context(gpu)?;
                frame.validate()?;
                cuda_nv12(frame, gpu)
            }
            ChainFrame::Host(f) => {
                f.validate()?;
                host_nv12(f)
            }
        };
        raw.flags = flags;
        // SAFETY: in place. A CUDA frame is device memory of the current
        // context (the seam's contract); a host frame's planes are valid for
        // reads and writes of their extent and accessed by nothing else
        // during the call (`HostNv12`'s contract), and `frame` holds its
        // only borrow.
        let result = unsafe { self.instance.process(&raw, &raw, &live) };
        result.map_err(|e| backend_error(e, Phase::Run))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use obc_backend::ColourSpec;

    fn request(effect: EffectKind, stages: &[StageId]) -> LoadRequest {
        LoadRequest {
            effect,
            stages: StageList::new(stages.iter().cloned()).unwrap(),
            width: 640,
            height: 360,
            colour: ColourSpec::default(),
            background: None,
            params: BTreeMap::new(),
        }
    }

    #[test]
    fn the_background_stage_goes_after_the_pre_matte_stages() {
        let slots: BTreeMap<StageId, Slot> = [
            (StageId::DENOISE, Slot::PreMatte),
            (StageId::BACKGROUND, Slot::Matte),
            (StageId::RELIGHT, Slot::PostMatte),
        ]
        .into_iter()
        .collect();
        let both = [StageId::DENOISE, StageId::RELIGHT];
        assert_eq!(
            effect_chain(&request(EffectKind::Blur, &both), &slots),
            [StageId::DENOISE, StageId::BACKGROUND, StageId::RELIGHT]
        );
        assert_eq!(
            effect_chain(&request(EffectKind::None, &both), &slots),
            both
        );
        assert_eq!(
            effect_chain(&request(EffectKind::Replace, &[StageId::RELIGHT]), &slots),
            [StageId::BACKGROUND, StageId::RELIGHT]
        );
        assert_eq!(
            effect_chain(&request(EffectKind::Blur, &[StageId::DENOISE]), &slots),
            [StageId::DENOISE, StageId::BACKGROUND]
        );
        assert_eq!(
            effect_chain(&request(EffectKind::Blur, &[]), &slots),
            [StageId::BACKGROUND]
        );
        // The slots win over the request's order.
        assert_eq!(
            effect_chain(
                &request(EffectKind::Blur, &[StageId::RELIGHT, StageId::DENOISE]),
                &slots
            ),
            [StageId::DENOISE, StageId::BACKGROUND, StageId::RELIGHT]
        );
        assert_eq!(
            effect_chain(
                &request(EffectKind::None, &[StageId::RELIGHT, StageId::DENOISE]),
                &slots
            ),
            [StageId::DENOISE, StageId::RELIGHT]
        );
    }

    #[test]
    fn errors_keep_the_seams_meaning() {
        let e = |status, stage: Option<StageId>| ModuleError {
            status,
            code: 7,
            message: "why".into(),
            stage,
        };
        assert_eq!(
            backend_error(e(Status::Stage, Some(StageId::RELIGHT)), Phase::Run),
            BackendError::Stage {
                stage: StageId::RELIGHT,
                code: 7,
                message: "why".into()
            }
        );
        assert_eq!(
            backend_error(e(Status::Invalid, None), Phase::Load),
            BackendError::InvalidInput("why".into())
        );
        assert_eq!(
            backend_error(e(Status::State, None), Phase::Run),
            BackendError::InvalidInput("why".into())
        );
        assert_eq!(
            backend_error(e(Status::Unavailable, None), Phase::Load),
            BackendError::Unavailable("why".into())
        );
        assert!(matches!(
            backend_error(e(Status::Failed, None), Phase::Load),
            BackendError::Load { code: 7, .. }
        ));
        assert!(matches!(
            backend_error(e(Status::Panic, None), Phase::Run),
            BackendError::Run { code: 7, .. }
        ));
    }

    #[test]
    fn params_are_checked_against_the_description() {
        let doc = serde_json::json!({"groups":[{"id":"lighting","label":"Lighting","settings":[
            {"id":"effect.lighting.relight","label":"Relighting","type":"bool","default":false,
             "stage":"relight","role":"enable"},
            {"id":"effect.lighting.light","label":"Light","type":"enum",
             "options":[["warm","Warm"],["cool","Cool"]],"default":"warm","stage":"relight",
             "key":"light","apply":"reload"},
            {"id":"effect.lighting.direction","label":"Direction","type":"int","min":-90,
             "max":90,"step":5,"default":0,"stage":"relight","key":"direction","apply":"live"}]}]});
        let s =
            crate::settings::parse_settings(&doc.to_string(), &[StageId::RELIGHT], |_| {}).unwrap();
        let p = |pairs: &[(&str, Value)]| -> Params {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect()
        };
        let r = StageId::RELIGHT;
        assert!(
            check_params(
                &s,
                &r,
                &p(&[("light", "cool".into())]),
                &p(&[("direction", 45.into())])
            )
            .is_ok()
        );
        assert!(check_params(&s, &r, &p(&[("other", true.into())]), &Params::new()).is_ok());
        let e = check_params(&s, &r, &p(&[("light", "red".into())]), &Params::new()).unwrap_err();
        assert!(e.contains("not one of warm, cool"), "{e}");
        let e = check_params(&s, &r, &Params::new(), &p(&[("direction", 120.into())])).unwrap_err();
        assert!(e.contains("outside -90 to 90"), "{e}");
        let e =
            check_params(&s, &r, &Params::new(), &p(&[("direction", "up".into())])).unwrap_err();
        assert!(e.contains("must be a number"), "{e}");
        assert!(
            check_params(
                &s,
                &StageId::DENOISE,
                &p(&[("light", 3.into())]),
                &Params::new()
            )
            .is_ok()
        );
    }

    #[test]
    fn names_are_interned_once() {
        let a = intern("org.x.a");
        let b = intern(&String::from("org.x.a"));
        assert!(std::ptr::eq(a, b));
        assert_eq!(intern("org.x.b"), "org.x.b");
    }
}
