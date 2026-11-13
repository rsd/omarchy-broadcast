//! The open-model cut-out: an omarchy-broadcast module that provides the
//! `background` stage (the person cut out, the room blurred or replaced)
//! with open ONNX models run by ONNX Runtime, so the background effect
//! works without NVIDIA Maxine.
//!
//! # The stage
//!
//! `background`, in the `matte` slot, on the CPU (its manifest's `device`;
//! the engine may be a GPU or an NPU, see [`engine`]). The provider of
//! `background` composites itself: the host passes the load-time parameter
//! `mode` (`blur` or `replace`), the live parameter `blur_strength` (a
//! float from 0 to 1) and, for `replace`, the background image as a host
//! BGR frame in the chain request. NV12 frames in and out, of one even
//! size, in CUDA memory (what the effect element hands it today: each frame
//! is copied to the host, processed and copied back, [`cuda`]) or in host
//! memory.
//!
//! Per frame: the frame averaged down to the model's input
//! ([`image::preprocess`]), the model run, its output read as a matte
//! ([`image::matte_of_output`]) and smoothed lightly over time
//! ([`image::Ema`], forgotten on a new source and a temporal reset), then
//! the matte scaled up bilinearly in the composite ([`image::Compositor`]):
//! the person over the room blurred (an area average to a quarter size, a
//! blur weighted by how much of each pixel is room, scaled back up) or over
//! the replacement image, or over the last per-frame background (ABI 1.4,
//! a video background: each NV12 frame copied into a buffer kept for the
//! chain, a CUDA one through the frame buffer on the chain's stream). A
//! repeated frame (ABI 1.1) reuses the matte of the frame it repeats and
//! is composited again: the output is always written.
//!
//! # Settings
//!
//! The group `onnx` ("Open-model cut-out"): `effect.onnx.model` (the model
//! of [`catalogue::MODELS`]) and `effect.onnx.engine` (the engine of
//! [`engine::Engine`]), both bound to `background` and applied by reloading
//! the chain: a switch, the previous chain running until the new one is in
//! force.
//!
//! # Configuration
//!
//! The module's table, `[module."br.com.dias.onnx-cutout"]` (every key
//! optional):
//!
//! * `ort_library`: the ONNX Runtime library (`$ORT_DYLIB_PATH` wins);
//! * `model_dir`: the first model directory searched ([`model_files`]);
//! * `threads`: the CPU threads of a session (default 4);
//! * `_host`: the daemon's. `compute_capability` is accepted and ignored.
//!   `{"_host": {"cuda": false}}` (the video pipeline runs in system
//!   memory) makes the module make no CUDA call: `auto` skips TensorRT and
//!   CUDA, an explicit `tensorrt` or `cuda` engine fails the load, and a
//!   chain of CUDA frames is refused. Absent or `true`: CUDA as usual.
//!
//! # Failures
//!
//! A matte never fails open. Every failure of a frame is a failure of the
//! `background` stage ([`Error::stage`]), which the host takes as a failure
//! of the whole chain. A failed load leaves what was loaded as it was (the
//! host keeps the previous chain of a switch running; a first load or a
//! reload ends unloaded). An engine chosen explicitly that cannot run fails
//! the load with its reason.

pub mod catalogue;
pub mod cuda;
pub mod engine;
pub mod image;
pub mod model_files;
pub mod runtime;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use obc_module_sdk::{
    Apply, ChainRequest, ColourMatrix, Error, Frame, FrameKind, Group, Instance, LoadReport,
    Module, Op, Param, PixelFormat, ProbeReport, Setting, SettingsDescription, log, serde_json,
};
use ort::value::TensorRef;
use serde::Deserialize;

use crate::catalogue::ModelSpec;
use crate::engine::{Caches, CudaContextGuard, Engine, Started};
use crate::image::{Colour, Compositor, Ema, Mode, Planes};
use crate::runtime::LibrarySource;

/// The stage this module provides.
pub const STAGE: &str = "background";
/// The `background` stage's load-time parameter with the effect.
pub const BACKGROUND_MODE: &str = "mode";
/// The `background` stage's live parameter with the blur strength.
pub const BACKGROUND_BLUR_STRENGTH: &str = "blur_strength";
/// The stage's load-time parameter with the model ([`MODEL_SETTING`]).
pub const MODEL_KEY: &str = "model";
/// The stage's load-time parameter with the engine ([`ENGINE_SETTING`]).
pub const ENGINE_KEY: &str = "engine";
/// The settings group.
pub const GROUP: &str = "onnx";
/// The model setting.
pub const MODEL_SETTING: &str = "effect.onnx.model";
/// The engine setting.
pub const ENGINE_SETTING: &str = "effect.onnx.engine";
/// The daemon's background effect setting, which the settings' condition
/// reads.
pub const DAEMON_BACKGROUND_MODE: &str = "effect.background.mode";
/// Why the settings are greyed while the background effect is off.
pub const REASON_TURN_ON_BACKGROUND: &str = "Turn on the background effect to change this";
/// The blur strength before a frame gives one (the daemon's default).
pub const DEFAULT_BLUR_STRENGTH: f32 = 0.5;
/// The CPU threads of a session by default.
pub const DEFAULT_THREADS: usize = 4;
/// The weight of each new matte in the temporal smoothing.
pub const EMA_WEIGHT: f32 = 0.7;

/// The module's configuration table.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    ort_library: Option<PathBuf>,
    #[serde(default)]
    model_dir: Option<PathBuf>,
    #[serde(default)]
    threads: Option<usize>,
    #[serde(default, rename = "_host")]
    _host: Option<serde_json::Value>,
}

/// Whether the host's reserved `_host` key leaves CUDA on: only an explicit
/// `"cuda": false` turns it off.
fn host_allows_cuda(host: Option<&serde_json::Value>) -> bool {
    host.and_then(|h| h.get("cuda"))
        .and_then(serde_json::Value::as_bool)
        != Some(false)
}

/// The module's settings.
pub fn settings() -> SettingsDescription {
    let bound = |s: Setting, key: &str| {
        s.stage(STAGE)
            .key(key)
            .apply(Apply::Reload)
            .enabled_when(DAEMON_BACKGROUND_MODE, Op::Ne, "none")
            .disabled_reason(REASON_TURN_ON_BACKGROUND)
    };
    SettingsDescription::new().group(
        Group::new(GROUP, "Open-model cut-out")
            .setting(bound(
                Setting::enumeration(
                    MODEL_SETTING,
                    "Model",
                    catalogue::MODELS.iter().map(|m| (m.id, m.label)),
                    catalogue::DEFAULT_MODEL,
                ),
                MODEL_KEY,
            ))
            .setting(bound(
                Setting::enumeration(
                    ENGINE_SETTING,
                    "Runs on",
                    Engine::ALL.map(|e| (e.as_str(), e.label())),
                    Engine::Auto.as_str(),
                ),
                ENGINE_KEY,
            )),
    )
}

/// What the module and its instances share.
struct Shared {
    library: LibrarySource,
    model_dirs: Vec<PathBuf>,
    threads: usize,
    caches: Caches,
    /// False when the host turned CUDA off (`_host.cuda`).
    cuda: bool,
}

/// The module.
pub struct OnnxModule {
    shared: Arc<Shared>,
}

impl Module for OnnxModule {
    type Instance = OnnxInstance;

    fn init(config: serde_json::Value) -> Result<Self, Error> {
        let config: Config = if config.is_null() {
            Config::default()
        } else {
            serde_json::from_value(config)?
        };
        let threads = match config.threads {
            None => DEFAULT_THREADS,
            Some(n @ 1..=64) => n,
            Some(n) => {
                return Err(Error::invalid(format!(
                    "threads is {n}: it must be from 1 to 64"
                )));
            }
        };
        let cuda = host_allows_cuda(config._host.as_ref());
        let library = runtime::library_source(
            std::env::var_os(runtime::DYLIB_ENV),
            config.ort_library.as_deref(),
        );
        let model_dirs =
            model_files::search_dirs(config.model_dir.as_deref(), |n| std::env::var_os(n));
        let caches = Caches::from_env(|n| std::env::var_os(n));
        log::debug!(
            "ONNX cut-out: runtime {library:?}, model directories {model_dirs:?}, {threads} threads, CUDA {}",
            if cuda { "allowed" } else { "off" }
        );
        Ok(OnnxModule {
            shared: Arc::new(Shared {
                library,
                model_dirs,
                threads,
                caches,
                cuda,
            }),
        })
    }

    /// Files only: the runtime's library and some model file. One log line
    /// per engine that the files already rule out.
    fn probe(&self) -> ProbeReport {
        let s = &self.shared;
        let library = match runtime::find_library(&s.library, &runtime::loader_dirs()) {
            Ok(l) => l,
            Err(reason) => return ProbeReport::new().unavailable(STAGE, reason),
        };
        if !model_files::any_installed(catalogue::MODELS.iter(), &s.model_dirs) {
            let dirs: Vec<String> = s
                .model_dirs
                .iter()
                .map(|d| d.display().to_string())
                .collect();
            return ProbeReport::new().unavailable(
                STAGE,
                format!("No cut-out model installed (searched: {})", dirs.join(", ")),
            );
        }
        for (engine, reason) in engine::static_reasons(&library, self.shared.cuda) {
            log::info!("ONNX cut-out: {} unavailable: {reason}", engine.label());
        }
        ProbeReport::new().available(STAGE)
    }

    fn settings(&self) -> SettingsDescription {
        settings()
    }

    /// Opens ONNX Runtime and logs which engines its build has, one line
    /// per engine: `GetAvailableProviders` and the files beside the
    /// library only. No provider is registered and no session made, so no
    /// CUDA call is made (the GPU is first touched by a load).
    fn check(&self, stages: &[&str]) -> Result<(), Error> {
        if !stages.contains(&STAGE) {
            return Ok(());
        }
        let rt = runtime::runtime(&self.shared.library).map_err(Error::unavailable)?;
        let statics = engine::static_reasons(&rt.library, self.shared.cuda);
        for e in engine::AUTO_ORDER {
            let name = e.provider().expect("a concrete engine");
            let line = if e.uses_cuda() && !self.shared.cuda {
                format!("unavailable: {}", engine::REASON_CUDA_OFF)
            } else if !rt.has_provider(name) {
                format!("unavailable: {}", engine::REASON_NOT_IN_BUILD)
            } else if let Some((_, r)) = statics.iter().find(|(s, _)| *s == e) {
                format!("unavailable: {r}")
            } else if e == Engine::Cpu {
                "available".to_string()
            } else {
                // Only the build is known here: whether the device runs
                // it is found when the cut-out loads (no CUDA call before).
                "available in this ONNX Runtime build (the device is tried when the cut-out loads)"
                    .to_string()
            };
            log::info!("ONNX cut-out: {} {line}", e.label());
        }
        Ok(())
    }

    fn create_instance(&self) -> Result<OnnxInstance, Error> {
        Ok(OnnxInstance {
            shared: self.shared.clone(),
            chain: None,
            strength: DEFAULT_BLUR_STRENGTH,
            timings: Vec::new(),
        })
    }
}

/// The loaded chain.
struct Chain {
    kind: FrameKind,
    width: u32,
    height: u32,
    colour: Colour,
    /// The CUDA context and stream, as addresses (compared only).
    cuda: (usize, usize),
    model: &'static ModelSpec,
    wanted: Engine,
    started: Started,
    input: (u32, u32),
    mode: Mode,
    /// The per-frame background ([`Instance::set_background`]): the last
    /// frame copied, in force while `live_on`; the buffer is kept for the
    /// chain's life, so the frames after the first allocate nothing.
    live: Option<image::Nv12>,
    live_on: bool,
    tensor: Vec<f32>,
    matte: Vec<f32>,
    has_matte: bool,
    ema: Ema,
    compositor: Compositor,
    /// Freed first, by [`Drop`], in the frames' context (see there).
    staging: Option<cuda::Staging>,
    /// The frames' context, recorded until after the session is gone (the
    /// last field, so dropped last): the primary context is never reset
    /// while it is the frames' context.
    _frames: cuda::FramesContext,
}

impl Drop for Chain {
    /// The page-locked buffer is freed before the session: dropping the
    /// session may make the primary context current, or reset it, and
    /// `cuMemFreeHost` must run with the frames' context current, as the
    /// host leaves it around the unload.
    fn drop(&mut self) {
        drop(self.staging.take());
    }
}

impl Chain {
    /// The same frames, model and engine: a switch that reuses the session.
    fn reusable_for(&self, req: &Request) -> bool {
        self.kind == req.kind
            && (self.width, self.height) == (req.width, req.height)
            && self.colour == req.colour
            && self.cuda == req.cuda
            && self.model.id == req.model.id
            && self.wanted == req.engine
    }
}

/// A chain request, read and checked.
struct Request {
    kind: FrameKind,
    width: u32,
    height: u32,
    colour: Colour,
    /// The CUDA context and stream, as addresses (compared only).
    cuda: (usize, usize),
    model: &'static ModelSpec,
    engine: Engine,
    mode: Mode,
}

fn colour_of(req: &ChainRequest<'_>) -> Result<Colour, Error> {
    let k = match req.matrix() {
        ColourMatrix::Bt601 => Colour::BT601,
        ColourMatrix::Bt709 => Colour::BT709,
        ColourMatrix::Bt2020 => Colour::BT2020,
        ColourMatrix::Other(m) => {
            return Err(Error::unsupported(format!("unknown colour matrix {m}")));
        }
    };
    Ok(Colour::new(k, req.full_range()))
}

fn str_param<'a>(req: &ChainRequest<'a>, key: &str) -> Result<Option<&'a str>, Error> {
    match req.param(STAGE, key) {
        None => Ok(None),
        Some(v) => v
            .as_str()
            .map(Some)
            .ok_or_else(|| Error::invalid(format!("the {STAGE} parameter {key} must be a string"))),
    }
}

/// Reads and checks a chain request.
fn read_request(req: &ChainRequest<'_>) -> Result<Request, Error> {
    match req.stages() {
        [STAGE] => {}
        [] => {
            return Err(Error::unsupported(
                "an empty chain: this module runs the background stage",
            ));
        }
        other => {
            return Err(Error::unsupported(format!(
                "this module runs only the {STAGE} stage, not {}",
                other.join(", ")
            )));
        }
    }
    let kind = req.input_kind();
    if !matches!(kind, FrameKind::Cuda | FrameKind::Host) {
        return Err(Error::unsupported(format!(
            "{kind:?} frames are not supported"
        )));
    }
    if req.input_format() != PixelFormat::Nv12 || req.output_format() != PixelFormat::Nv12 {
        return Err(Error::unsupported(
            "the cut-out takes and gives NV12 frames",
        ));
    }
    let (width, height) = req.input_size();
    if req.output_size() != (width, height) {
        return Err(Error::unsupported("the cut-out keeps the frame's size"));
    }
    if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
        return Err(Error::unsupported(format!(
            "a {width}x{height} frame is not supported: the width and height must be even"
        )));
    }
    if kind == FrameKind::Cuda && req.cuda().is_null() {
        return Err(Error::invalid(
            "a CUDA chain needs the frames' CUDA context",
        ));
    }
    let colour = colour_of(req)?;
    let model = match str_param(req, MODEL_KEY)? {
        None => catalogue::default_model(),
        Some(id) => catalogue::find(id).ok_or_else(|| {
            let ids: Vec<&str> = catalogue::MODELS.iter().map(|m| m.id).collect();
            Error::invalid(format!("the model {id:?} is not one of {}", ids.join(", ")))
        })?,
    };
    let engine = match str_param(req, ENGINE_KEY)? {
        None => Engine::Auto,
        Some(e) => Engine::parse(e).ok_or_else(|| {
            let ids: Vec<&str> = Engine::ALL.iter().map(|e| e.as_str()).collect();
            Error::invalid(format!("the engine {e:?} is not one of {}", ids.join(", ")))
        })?,
    };
    let mode = match str_param(req, BACKGROUND_MODE)? {
        None | Some("blur") => Mode::Blur,
        Some("replace") => {
            let bg = req
                .background()
                .ok_or_else(|| Error::invalid("replace needs the background image"))?;
            Mode::Replace(background(bg, (width, height), &colour)?)
        }
        Some(other) => {
            return Err(Error::invalid(format!(
                "the {STAGE} mode {other:?} is not blur or replace"
            )));
        }
    };
    Ok(Request {
        kind,
        width,
        height,
        colour,
        cuda: (req.cuda().context as usize, req.cuda().stream as usize),
        model,
        engine,
        mode,
    })
}

/// The replacement image, at the frame's size, from the request's host
/// BGR frame (valid only during the load).
fn background(bg: &Frame<'_>, (w, h): (u32, u32), colour: &Colour) -> Result<image::Nv12, Error> {
    if bg.kind() != FrameKind::Host || bg.format() != PixelFormat::Bgr {
        return Err(Error::unsupported(
            "the background image must be a host BGR frame",
        ));
    }
    if bg.width() == 0 || bg.height() == 0 {
        return Err(Error::invalid("the background image is empty"));
    }
    // SAFETY: the host's frame, valid during the load; only read.
    let data = unsafe { bg.host_plane(0) }?;
    let pitch = bg.plane(0).map_or(0, |p| p.pitch as usize);
    Ok(image::background_nv12(
        data,
        pitch,
        (bg.width() as usize, bg.height() as usize),
        (w as usize, h as usize),
        colour,
    ))
}

/// The planes of a host NV12 frame of `width x height`; for the output
/// (`writable`), from the frame's writable view.
fn host_planes(f: &Frame<'_>, width: u32, height: u32, writable: bool) -> Result<Planes, Error> {
    if f.kind() != FrameKind::Host || f.format() != PixelFormat::Nv12 {
        return Err(Error::invalid("not a host NV12 frame"));
    }
    if (f.width(), f.height()) != (width, height) {
        return Err(Error::invalid(format!(
            "a {}x{} frame in a chain loaded for {width}x{height}",
            f.width(),
            f.height()
        )));
    }
    // SAFETY: the SDK checks each plane's span against its pitch and the
    // format's geometry; the host's frames are valid during the call, the
    // output writable and touched by nothing else meanwhile. The slices are
    // dropped at once, only the addresses are kept for this call.
    let (y, uv) = unsafe {
        if writable {
            (
                f.host_plane_mut(0)?.as_mut_ptr(),
                f.host_plane_mut(1)?.as_mut_ptr(),
            )
        } else {
            (
                f.host_plane(0)?.as_ptr().cast_mut(),
                f.host_plane(1)?.as_ptr().cast_mut(),
            )
        }
    };
    let p = Planes {
        y,
        y_pitch: f.plane(0).map_or(0, |p| p.pitch as usize),
        uv,
        uv_pitch: f.plane(1).map_or(0, |p| p.pitch as usize),
        width: width as usize,
        height: height as usize,
    };
    p.check().map_err(Error::invalid)?;
    Ok(p)
}

/// An instance: one chain for one stream.
pub struct OnnxInstance {
    shared: Arc<Shared>,
    chain: Option<Chain>,
    strength: f32,
    timings: Vec<(String, f32)>,
}

impl OnnxInstance {
    /// Loads a new chain for `req`, or reuses the session of the loaded one.
    fn build(&mut self, req: Request) -> Result<(Chain, LoadReport), Error> {
        let t0 = Instant::now();
        let input = req.model.input_size(req.width, req.height);
        let (w, h) = (req.width as usize, req.height as usize);
        if let Some(old) = self.chain.take_if(|c| c.reusable_for(&req)) {
            // A switch: only the effect or its image changes.
            let mut c = old;
            c.mode = req.mode;
            // The new load-time image is in force until the next frame.
            c.live_on = false;
            c.ema.reset();
            c.has_matte = false;
            let report = LoadReport {
                load_ms: t0.elapsed().as_secs_f32() * 1e3,
                warmup_ms: None,
                reused_model: true,
            };
            return Ok((c, report));
        }
        let s = &self.shared;
        let model = model_files::resolve(req.model, &s.model_dirs).map_err(Error::unavailable)?;
        let rt = runtime::runtime(&s.library).map_err(Error::unavailable)?;
        if req.kind == FrameKind::Cuda && !s.cuda {
            return Err(Error::unsupported(format!(
                "CUDA frames are not supported: {}",
                engine::REASON_CUDA_OFF
            )));
        }
        // Recorded before any session is made, dropped after it.
        let frames = cuda::FramesContext::register(match req.kind {
            FrameKind::Cuda => req.cuda.0,
            _ => 0,
        });
        // The frame buffer first, in the frames' context: a failure here
        // leaves no session to drop.
        let staging = match req.kind {
            FrameKind::Cuda => Some(cuda::Staging::new(req.width, req.height)?),
            _ => None,
        };
        let engine_req = engine::Request {
            runtime: &rt,
            model: req.model,
            bytes: &model.bytes,
            input,
            threads: s.threads,
            caches: &s.caches,
            switch: self.chain.is_some(),
            cuda: s.cuda,
        };
        let (started, _reasons) =
            engine::start(&engine_req, req.engine).map_err(Error::unavailable)?;
        log::debug!(
            "ONNX cut-out: {} from {}",
            req.model.id,
            model.path.display()
        );
        if started.built_tensorrt_engine {
            log::info!(
                "ONNX cut-out: the TensorRT engine for {} is ready ({:.1} s, cached for next time)",
                req.model.id,
                (started.load_ms + started.warmup_ms) / 1e3
            );
        }
        let plane = input.0 as usize * input.1 as usize;
        let report = LoadReport {
            load_ms: t0.elapsed().as_secs_f32() * 1e3,
            warmup_ms: Some(started.warmup_ms),
            reused_model: false,
        };
        let chain = Chain {
            kind: req.kind,
            width: req.width,
            height: req.height,
            colour: req.colour,
            cuda: req.cuda,
            model: req.model,
            wanted: req.engine,
            started,
            input,
            mode: req.mode,
            live: None,
            live_on: false,
            tensor: vec![0.0; 3 * plane],
            matte: vec![0.0; plane],
            has_matte: false,
            ema: Ema::new(EMA_WEIGHT),
            compositor: Compositor::new(
                w,
                h,
                (input.0 as usize, input.1 as usize),
                image::workers(),
            ),
            staging,
            _frames: frames,
        };
        Ok((chain, report))
    }

    fn drop_chain(&mut self) {
        if let Some(c) = self.chain.take() {
            // Releasing a GPU session may change the current CUDA context.
            let _ctx = CudaContextGuard::save(c.started.engine);
            drop(c);
        }
    }

    /// Runs one frame; any error is the stage's.
    fn run(&mut self, input: &Frame<'_>, output: &Frame<'_>) -> Result<(), String> {
        let strength = self.strength;
        let c = self.chain.as_mut().ok_or("no chain is loaded")?;
        for f in [input, output] {
            if f.kind() != c.kind || f.format() != PixelFormat::Nv12 {
                return Err(format!(
                    "a {:?} {:?} frame in a chain loaded for {:?} NV12",
                    f.kind(),
                    f.format(),
                    c.kind
                ));
            }
            if (f.width(), f.height()) != (c.width, c.height) {
                return Err(format!(
                    "a {}x{} frame in a chain loaded for {}x{}",
                    f.width(),
                    f.height(),
                    c.width,
                    c.height
                ));
            }
        }
        if input.is_new_source() {
            c.ema.reset();
            c.has_matte = false;
        }
        let mut timings = Vec::with_capacity(4);
        let t = Instant::now();
        let (src, dst) = match c.kind {
            FrameKind::Cuda => {
                let staging = c.staging.as_mut().ok_or("no frame buffer")?;
                let host = staging.download(input).map_err(|e| e.to_string())?;
                let p = Planes {
                    y: host.y,
                    y_pitch: host.y_pitch,
                    uv: host.uv,
                    uv_pitch: host.uv_pitch,
                    width: c.width as usize,
                    height: c.height as usize,
                };
                (p, p)
            }
            _ => {
                let src =
                    host_planes(input, c.width, c.height, false).map_err(|e| e.to_string())?;
                let dst =
                    host_planes(output, c.width, c.height, true).map_err(|e| e.to_string())?;
                if !src.same(&dst) && input.overlaps(output) {
                    return Err("the output frame overlaps the input".into());
                }
                (src, dst)
            }
        };
        // A repeat has the picture of the frame before, whose matte is
        // kept: it is composited again, with no new matte.
        let fresh = !(input.is_repeat() && c.has_matte);
        if fresh {
            image::preprocess(&src, &c.colour, c.model, c.input, &mut c.tensor);
        }
        timings.push(("convert-in".to_string(), t.elapsed().as_secs_f32() * 1e3));
        let t = Instant::now();
        if fresh {
            let shape = c.model.input_shape(c.input);
            let _ctx = CudaContextGuard::save(c.started.engine);
            let tensor = TensorRef::from_array_view((shape, c.tensor.as_slice()))
                .map_err(|e| format!("the model's input: {e}"))?;
            let name = c.started.input_name.as_str();
            let outputs = c
                .started
                .session
                .run(ort::inputs![name => tensor])
                .map_err(|e| format!("the model failed: {e}"))?;
            let (_, values) = outputs[0]
                .try_extract_tensor::<f32>()
                .map_err(|e| format!("the model's output: {e}"))?;
            if values.len() != c.model.output_len(c.input) {
                return Err(format!("the model gave {} values", values.len()));
            }
            image::matte_of_output(c.model, values, c.input, &mut c.matte);
            c.ema.apply(&mut c.matte);
            c.has_matte = true;
        }
        timings.push(("mask".to_string(), t.elapsed().as_secs_f32() * 1e3));
        let t = Instant::now();
        match (&c.mode, c.live.as_ref()) {
            (Mode::Replace(_), Some(live)) if c.live_on => {
                c.compositor.replace(&src, &dst, &c.matte, live);
            }
            (mode, _) => c.compositor.composite(&src, &dst, &c.matte, mode, strength),
        }
        timings.push(("composite".to_string(), t.elapsed().as_secs_f32() * 1e3));
        let t = Instant::now();
        if c.kind == FrameKind::Cuda {
            let staging = c.staging.as_mut().ok_or("no frame buffer")?;
            staging.upload(output).map_err(|e| e.to_string())?;
        }
        timings.push(("convert-out".to_string(), t.elapsed().as_secs_f32() * 1e3));
        self.timings = timings;
        Ok(())
    }

    /// Keeps `frame` (NV12 of the chain's kind and size, checked by the
    /// SDK) as the background of the following frames. A CUDA frame is
    /// copied to the host through the frame buffer, which is free between
    /// frames (the last upload is ordered before the copy on the chain's
    /// stream). Nothing changes on a failure.
    fn keep_background(&mut self, frame: &Frame<'_>) -> Result<(), Error> {
        let c = self
            .chain
            .as_mut()
            .ok_or_else(|| Error::state("no chain is loaded"))?;
        if !matches!(c.mode, Mode::Replace(_)) {
            return Err(Error::state(
                "the chain blurs the room: it takes no background frame",
            ));
        }
        let src = match c.kind {
            FrameKind::Cuda => {
                if frame.cuda().stream as usize != c.cuda.1 {
                    return Err(Error::invalid(
                        "the background frame is not on the chain's CUDA stream",
                    ));
                }
                let staging = c
                    .staging
                    .as_mut()
                    .ok_or_else(|| Error::state("no frame buffer"))?;
                let host = staging
                    .download(frame)
                    .map_err(|e| Error::new(e.kind(), format!("background: {e}")))?;
                Planes {
                    y: host.y,
                    y_pitch: host.y_pitch,
                    uv: host.uv,
                    uv_pitch: host.uv_pitch,
                    width: c.width as usize,
                    height: c.height as usize,
                }
            }
            _ => host_planes(frame, c.width, c.height, false)
                .map_err(|e| Error::new(e.kind(), format!("background: {e}")))?,
        };
        let (w, h) = (c.width as usize, c.height as usize);
        let live = c.live.get_or_insert_with(|| image::Nv12::new(w, h));
        live.copy_from(&src)
            .map_err(|e| Error::invalid(format!("background: {e}")))?;
        c.live_on = true;
        Ok(())
    }
}

impl Instance for OnnxInstance {
    fn load(&mut self, req: &ChainRequest<'_>) -> Result<LoadReport, Error> {
        let request = read_request(req)?;
        // On failure the loaded chain stays as it was: the host keeps a
        // switch's previous chain running, and unloads after a failed
        // first load or reload.
        let (chain, report) = match self.build(request) {
            Ok(r) => r,
            Err(e) => {
                log::warn!("ONNX cut-out: the load failed: {e}");
                return Err(e);
            }
        };
        self.drop_chain();
        self.chain = Some(chain);
        self.strength = DEFAULT_BLUR_STRENGTH;
        self.timings.clear();
        Ok(report)
    }

    fn process(
        &mut self,
        input: &Frame<'_>,
        output: &Frame<'_>,
        live: &[Param<'_>],
    ) -> Result<(), Error> {
        if let Some(s) =
            Param::find(live, STAGE, BACKGROUND_BLUR_STRENGTH).and_then(|v| v.as_float())
        {
            self.strength = if s.is_finite() {
                s.clamp(0.0, 1.0) as f32
            } else {
                DEFAULT_BLUR_STRENGTH
            };
        }
        // A matte never fails open: any failure is the stage's, and the
        // chain is dropped with it.
        self.run(input, output).map_err(|e| {
            self.drop_chain();
            Error::stage(STAGE, e)
        })
    }

    /// A video background (ABI 1.4): host frames copied, CUDA frames
    /// copied to the host on the chain's stream (the composite is on the
    /// CPU either way).
    const LIVE_BACKGROUND: bool = true;

    fn set_background(&mut self, frame: Option<&Frame<'_>>) -> Result<(), Error> {
        match frame {
            Some(f) => self.keep_background(f),
            None => {
                let c = self
                    .chain
                    .as_mut()
                    .ok_or_else(|| Error::state("no chain is loaded"))?;
                c.live_on = false;
                Ok(())
            }
        }
    }

    fn reset_temporal(&mut self) -> Result<Vec<String>, Error> {
        if let Some(c) = self.chain.as_mut() {
            c.ema.reset();
            c.has_matte = false;
        }
        Ok(Vec::new())
    }

    fn unload(&mut self) {
        self.drop_chain();
        self.strength = DEFAULT_BLUR_STRENGTH;
        self.timings.clear();
    }

    fn timings(&self) -> Vec<(String, f32)> {
        self.timings.clone()
    }
}

impl Drop for OnnxInstance {
    fn drop(&mut self) {
        self.drop_chain();
    }
}

obc_module_sdk::export_module!(OnnxModule);

#[cfg(test)]
mod tests {
    use super::*;
    use obc_module_sdk::{Role, SettingKind};
    use serde_json::json;

    #[test]
    fn the_settings_are_the_model_and_the_engine_of_the_background() {
        let d = settings();
        assert_eq!(d.groups.len(), 1);
        let g = &d.groups[0];
        assert_eq!(
            (g.id.as_str(), g.label.as_str()),
            ("onnx", "Open-model cut-out")
        );
        let ids: Vec<&str> = g.settings.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, [MODEL_SETTING, ENGINE_SETTING]);
        for (s, key) in g.settings.iter().zip([MODEL_KEY, ENGINE_KEY]) {
            assert_eq!(s.stage.as_deref(), Some(STAGE));
            assert_eq!(s.key.as_deref(), Some(key));
            assert_eq!(s.apply, Some(Apply::Reload));
            assert_eq!(s.role, None::<Role>);
            assert_eq!(s.enabled_when.len(), 1);
            assert!(s.id.len() <= 48 && s.label.len() <= 48);
        }
        match &g.settings[0].kind {
            SettingKind::Enum { options, default } => {
                assert_eq!(default, "mediapipe-landscape");
                assert_eq!(
                    options,
                    &[
                        ("mediapipe-landscape".into(), "MediaPipe (fast)".into()),
                        (
                            "pphumanseg-v2-mobile".into(),
                            "PP-HumanSeg mobile (quality)".into()
                        ),
                        ("pphumanseg-v2-lite".into(), "PP-HumanSeg lite".into()),
                        ("modnet".into(), "MODNet (sharp hair, GPU)".into()),
                    ]
                );
            }
            k => panic!("{k:?}"),
        }
        match &g.settings[1].kind {
            SettingKind::Enum { options, default } => {
                assert_eq!(default, "auto");
                let values: Vec<&str> = options.iter().map(|(v, _)| v.as_str()).collect();
                assert_eq!(
                    values,
                    [
                        "auto",
                        "tensorrt",
                        "cuda",
                        "openvino-npu",
                        "openvino-gpu",
                        "openvino-cpu",
                        "cpu"
                    ]
                );
            }
            k => panic!("{k:?}"),
        }
        // The JSON the daemon reads.
        let v: serde_json::Value = serde_json::from_str(&d.to_json()).unwrap();
        let model = &v["groups"][0]["settings"][0];
        assert_eq!(model["type"], "enum");
        assert_eq!(model["apply"], "reload");
        assert_eq!(
            model["enabled_when"],
            json!([[DAEMON_BACKGROUND_MODE, "ne", "none"]])
        );
    }

    #[test]
    fn the_configuration_is_checked() {
        OnnxModule::init(json!({})).unwrap();
        OnnxModule::init(serde_json::Value::Null).unwrap();
        let m = OnnxModule::init(json!({
            "model_dir": "/m", "threads": 2, "ort_library": "/x/libonnxruntime.so",
            "_host": {"compute_capability": [12, 0]}
        }))
        .unwrap();
        assert_eq!(m.shared.threads, 2);
        assert_eq!(m.shared.model_dirs[0], PathBuf::from("/m"));
        assert!(m.shared.cuda);
        for (host, cuda) in [
            (json!({"cuda": false}), false),
            (json!({"cuda": false, "compute_capability": [12, 0]}), false),
            (json!({"cuda": true}), true),
            (json!({}), true),
            (json!({"cuda": "no"}), true),
        ] {
            let m = OnnxModule::init(json!({"_host": host})).unwrap();
            assert_eq!(m.shared.cuda, cuda, "{host}");
        }
        assert!(OnnxModule::init(json!({"nope": 1})).is_err());
        assert!(OnnxModule::init(json!({"threads": 0})).is_err());
    }
}
