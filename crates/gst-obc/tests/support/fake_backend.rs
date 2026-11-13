//! A fake inference backend for the effect tests: no models, a
//! configurable load delay and failures, and an output that is easy to
//! recognise: `blur` writes Y = [`FAKE_BLUR_Y`], `replace` Y =
//! [`FAKE_REPLACE_Y`], the denoise stage alone Y = [`FAKE_DENOISE_Y`], the
//! relight stage alone (with or without denoise) Y = [`FAKE_RELIGHT_Y`],
//! and the relight stage under an effect adds [`FAKE_RELIGHT_DY`] to the
//! effect's Y; all with neutral chroma unless the denoise stage ran, which
//! writes chroma [`FAKE_DENOISE_UV`] instead, through `obc-gpu` on the
//! stream it is given.
//! Its stages are `denoise` and `relight`, with the relight stage's
//! parameters read as the Maxine backend reads them (`light` at load time,
//! `direction` and `brightness` per frame).
//! Like Maxine, its `reset_temporal` drops a loaded relight stage and returns
//! it (unless `FakeConfig::relight_resets_in_place`); each build of the
//! stage is numbered, so a test can tell which one relit a frame.
//! It records every call and whether the caller kept the context and stream
//! rules of `obc_backend::InferenceBackend`. Like a real backend, it reports
//! a frame's stage timings (`timings`) only once that frame's work has
//! completed on the GPU (a CUDA event recorded after it), so a caller that
//! reads them before synchronising the stream gets none. While the denoise
//! stage is loaded it reports the status value `denoise`/`level`
//! ([`FAKE_DENOISE_LEVEL`]), while an effect is loaded `background`/
//! `coverage` ([`FAKE_COVERAGE`]), and whenever it is loaded a value of a
//! stage it never loads ([`FAKE_STRAY_STAGE`]).
//!
//! It takes host frames too (`load_host`, `process_host`,
//! `reset_temporal_host`, the factory's `kinds` listing both memories unless
//! made with [`FakeFactory::cuda_only`]): the same behaviour and output,
//! written on the CPU, with no CUDA call at all, so the system-memory tests
//! run without a GPU. One difference: on host frames the first Y row is the
//! input's first Y row plus one (saturating), so the output depends on the
//! input. It records a call of one memory's methods on an
//! instance loaded on the other ([`FakeLog::wrong_memory`]).
//!
//! Shared by the tests of `gst-obc` (the GPU ones and the system-memory
//! ones of `tests/element.rs`) and `obc-pipeline` (included with `#[path]`).

#![allow(dead_code)] // each including test uses a different subset

use std::ffi::c_void;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use obc_backend::{
    BackendError, BackendFactory, DeviceNv12, EffectKind, FrameKind, FrameParams, GpuContext,
    HostNv12, InferenceBackend, LoadReport, LoadRequest, Params, StageId, StageList, StageTimings,
    StageValue,
};
// The fake takes the relight stage's parameters as the Maxine backend does.
use obc_backend_maxine::{RELIGHT_LIGHT, RelightLight, relight_frame};
use obc_gpu::Event;

/// The luma the fake blur writes.
pub const FAKE_BLUR_Y: u8 = 200;
/// The luma the fake replace writes.
pub const FAKE_REPLACE_Y: u8 = 100;
/// The luma the fake denoise stage writes when it runs alone.
pub const FAKE_DENOISE_Y: u8 = 60;
/// The chroma (both U and V) the fake writes when the denoise stage ran.
pub const FAKE_DENOISE_UV: u8 = 90;
/// The status value `denoise`/`level` the fake reports while the denoise
/// stage is loaded.
pub const FAKE_DENOISE_LEVEL: f64 = 0.5;
/// The status value `background`/`coverage` the fake reports while an
/// effect (blur or replace) is loaded.
pub const FAKE_COVERAGE: f64 = 0.25;
/// The stage of the status value `stray` the fake reports whenever it is
/// loaded: a stage it never loads, so the element must leave it out.
pub const FAKE_STRAY_STAGE: StageId = StageId::ENHANCE;
/// The denoise stage time the fake reports (`timings`), in ms, for a
/// completed frame the stage ran on.
pub const FAKE_DENOISE_MS: f32 = 0.25;
/// The luma the fake relight stage writes when it runs without an effect.
pub const FAKE_RELIGHT_Y: u8 = 150;
/// What the fake relight stage adds to an effect's luma.
pub const FAKE_RELIGHT_DY: u8 = 10;
/// The relight stage time the fake reports, as [`FAKE_DENOISE_MS`].
pub const FAKE_RELIGHT_MS: f32 = 0.5;

/// A test's action, run by the fake on the streaming thread (see
/// [`FakeConfig::on_process`]).
#[derive(Clone)]
pub struct Hook(pub Arc<dyn Fn() + Send + Sync>);

impl Hook {
    pub fn new(f: impl Fn() + Send + Sync + 'static) -> Hook {
        Hook(Arc::new(f))
    }
}

impl fmt::Debug for Hook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Hook")
    }
}

/// How the fake behaves; changeable while it runs.
#[derive(Debug, Clone, Default)]
pub struct FakeConfig {
    /// Sleep of a load on an unloaded instance (a cold model load).
    pub load_delay: Duration,
    /// Sleep of a load on a loaded instance (a switch between effects). A
    /// load of an effect on an instance with only the denoise stage loaded
    /// is cold (the green screen loads): `load_delay`.
    pub switch_delay: Duration,
    /// `probe` and `create` fail with this reason (backend unavailable).
    pub fail_create: Option<String>,
    /// `load` fails with this reason.
    pub fail_load: Option<String>,
    /// `process` fails with this reason (after `fail_denoise_process`, so
    /// with both set a frame fails in the stage, then again without it).
    pub fail_process: Option<String>,
    /// A `load` with the denoise stage fails with `BackendError::Stage` and this
    /// reason (nothing of the request in force, as the seam says).
    pub fail_denoise_load: Option<String>,
    /// A `process` with the denoise stage loaded fails with
    /// `BackendError::Stage` and this reason, and drops the stage.
    pub fail_denoise_process: Option<String>,
    /// A `load` with the relight stage fails with `BackendError::Stage`
    /// for it and this reason (after `fail_denoise_load`).
    pub fail_relight_load: Option<String>,
    /// A `load` on a loaded instance that gives the loaded relight stage a
    /// new light fails with `BackendError::Stage` for it and this reason,
    /// and, as the seam says of that switch, drops the stage: the previous
    /// chain stays, without it.
    pub fail_relight_rebind: Option<String>,
    /// Run once (then cleared) when `fail_relight_rebind` fails a load,
    /// before the error is returned, on the caller's (worker) thread: a
    /// test's action between a failed switch and what the element does
    /// next.
    pub on_rebind_failure: Option<Hook>,
    /// `reset_temporal` keeps a loaded relight stage instead of dropping
    /// and returning it (by default the fake drops it, as Maxine does,
    /// whose relighting state cannot be reset).
    pub relight_resets_in_place: bool,
    /// A `process` with the relight stage loaded fails with
    /// `BackendError::Stage` for it and this reason, and drops the stage
    /// (after `fail_denoise_process`, before `fail_process`).
    pub fail_relight_process: Option<String>,
    /// The next `process` (then cleared) fails with `BackendError::Run` and
    /// this reason, and the fake loses its chain on its own, as a module
    /// instance does after a panic: unloaded, `is_loaded` false, without
    /// the caller having asked (checked before the failures above).
    pub lose_chain_on_process: Option<String>,
    /// Run once at the start of the next `process` (then cleared), before
    /// the failures above are checked: a test's action while a frame is in
    /// the backend (the element's streaming thread holds its engine lock).
    pub on_process: Option<Hook>,
}

/// What the fake saw.
#[derive(Debug, Default)]
pub struct FakeLog {
    pub creates: AtomicU64,
    pub loads: AtomicU64,
    pub unloads: AtomicU64,
    pub processes: AtomicU64,
    pub drops: AtomicU64,
    /// Calls made without the request's context current.
    pub wrong_context: AtomicU64,
    /// `process` calls on another stream than the one loaded with.
    pub wrong_stream: AtomicU64,
    /// Background sizes received by replace loads.
    pub backgrounds: Mutex<Vec<(u32, u32)>>,
    /// The last blur strength processed, in thousandths.
    pub last_strength_milli: AtomicU64,
    /// Effects loaded, in order.
    pub loaded: Mutex<Vec<EffectKind>>,
    /// Successful loads as (effect, denoise), in order.
    pub requests: Mutex<Vec<(EffectKind, bool)>>,
    /// `reset_temporal` calls.
    pub resets: AtomicU64,
    /// `process` calls that ran the denoise stage.
    pub denoised: AtomicU64,
    /// `process` calls that ran the relight stage.
    pub relit: AtomicU64,
    /// Successful loads as (effect, stages, relight light), in order.
    pub stage_requests: Mutex<Vec<(EffectKind, Vec<StageId>, RelightLight)>>,
    /// The relight direction and gain of the last frame that ran the
    /// relight stage, in thousandths.
    pub last_relight_milli: Mutex<Option<(i64, i64)>>,
    /// Relight stages built (a load that adds the stage; a new light on a
    /// loaded one builds none). The first is 1.
    pub relight_builds: AtomicU64,
    /// For each `process` call that ran the relight stage, the build
    /// ([`Self::relight_builds`]) that relit it, in order.
    pub relit_by: Mutex<Vec<u64>>,
    /// `reset_temporal` calls that dropped the relight stage.
    pub relight_resets_dropped: AtomicU64,
    /// Denoise stages built (a successful load that adds the stage; one
    /// that keeps a loaded stage builds none): the denoise model loads.
    pub denoise_builds: AtomicU64,
    /// Successful loads that gave the loaded relight stage a new light (a
    /// live rebind, no model load).
    pub relight_rebinds: AtomicU64,
    /// Chains lost on their own ([`FakeConfig::lose_chain_on_process`]).
    pub lost_chains: AtomicU64,
    /// Successful `load_host` calls (counted in [`Self::loads`] too).
    pub host_loads: AtomicU64,
    /// Successful `process_host` calls (counted in [`Self::processes`]
    /// too).
    pub host_processes: AtomicU64,
    /// Calls of the CUDA methods on an instance loaded on host frames, or
    /// of the host methods on one loaded on CUDA frames.
    pub wrong_memory: AtomicU64,
}

impl FakeLog {
    pub fn get(a: &AtomicU64) -> u64 {
        a.load(Ordering::SeqCst)
    }
}

/// The factory; clone the `Arc`s to change the config or read the log.
pub struct FakeFactory {
    pub config: Arc<Mutex<FakeConfig>>,
    pub log: Arc<FakeLog>,
    /// What `kinds` says.
    kinds: Vec<FrameKind>,
}

impl FakeFactory {
    /// A fake that takes CUDA and host frames.
    pub fn new(config: FakeConfig) -> Arc<FakeFactory> {
        FakeFactory::with_kinds(config, vec![FrameKind::Cuda, FrameKind::Host])
    }

    /// A fake whose factory says it takes CUDA frames only (its instances
    /// would still take host frames if asked).
    pub fn cuda_only(config: FakeConfig) -> Arc<FakeFactory> {
        FakeFactory::with_kinds(config, vec![FrameKind::Cuda])
    }

    fn with_kinds(config: FakeConfig, kinds: Vec<FrameKind>) -> Arc<FakeFactory> {
        Arc::new(FakeFactory {
            config: Arc::new(Mutex::new(config)),
            log: Arc::default(),
            kinds,
        })
    }

    pub fn set(&self, f: impl FnOnce(&mut FakeConfig)) {
        f(&mut self.config.lock().unwrap());
    }

    fn cfg(&self) -> FakeConfig {
        self.config.lock().unwrap().clone()
    }
}

impl BackendFactory for FakeFactory {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn check_params(&self, stage: &StageId, params: &Params, live: &Params) -> Result<(), String> {
        obc_backend_maxine::check_stage_params(stage.as_str(), params, live)
    }

    fn kinds(&self) -> &[FrameKind] {
        &self.kinds
    }

    fn probe(&self) -> Result<(), BackendError> {
        match self.cfg().fail_create {
            Some(r) => Err(BackendError::Unavailable(r)),
            None => Ok(()),
        }
    }

    fn create(&self) -> Result<Box<dyn InferenceBackend>, BackendError> {
        self.probe()?;
        self.log.creates.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(FakeBackend {
            config: Arc::clone(&self.config),
            log: Arc::clone(&self.log),
            loaded: None,
            denoise: false,
            relight: false,
            light: RelightLight::default(),
            relight_build: 0,
            stream: 0,
            context: 0,
            done: None,
            timed: (false, false),
            host: false,
            host_done: false,
        }))
    }
}

pub struct FakeBackend {
    config: Arc<Mutex<FakeConfig>>,
    log: Arc<FakeLog>,
    loaded: Option<EffectKind>,
    /// The denoise stage is loaded (with `loaded`).
    denoise: bool,
    /// The relight stage is loaded (with `loaded`).
    relight: bool,
    /// Its light, and which build of it is loaded
    /// ([`FakeLog::relight_builds`]).
    light: RelightLight,
    relight_build: u64,
    /// The stream and context of the last load (as addresses: Send).
    stream: usize,
    context: usize,
    /// Recorded on the stream after each frame's work (created with the
    /// first frame, in the loaded context; dropped on unload).
    done: Option<Event>,
    /// The stages that ran on the last frame (denoise, relight).
    timed: (bool, bool),
    /// The last load was `load_host`: no CUDA call is made for this
    /// instance (no context check, no event).
    host: bool,
    /// The last frame was processed on the host: its work is complete.
    host_done: bool,
}

impl FakeBackend {
    fn check_context(&self, expected: *mut c_void) {
        let current = obc_gpu::current_context().unwrap_or(std::ptr::null_mut());
        if current.is_null() || current != expected {
            self.log.wrong_context.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Records a call of the `host` memory's methods on an instance
    /// loaded on the other one.
    fn check_memory(&self, host: bool) {
        if self.loaded.is_some() && self.host != host {
            self.log.wrong_memory.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// `load` and `load_host`: `gpu` is the CUDA context and stream, `None`
    /// for host frames.
    fn load_on(
        &mut self,
        gpu: Option<&GpuContext>,
        req: &LoadRequest,
    ) -> Result<LoadReport, BackendError> {
        let denoise = req.stages.contains("denoise");
        let relight = req.stages.contains("relight");
        let light = req
            .stage_params("relight")
            .and_then(|p| p.get(RELIGHT_LIGHT))
            .and_then(|v| v.as_str())
            .map_or(Ok(RelightLight::default()), str::parse)?;
        let cfg = self.config.lock().unwrap().clone();
        let t0 = Instant::now();
        let reused = self.loaded.is_some();
        let cold =
            !reused || (self.loaded == Some(EffectKind::None) && req.effect != EffectKind::None);
        std::thread::sleep(if cold {
            cfg.load_delay
        } else {
            cfg.switch_delay
        });
        if let Some(r) = cfg.fail_load {
            if !reused {
                self.loaded = None;
                self.denoise = false;
                self.relight = false;
            }
            return Err(BackendError::Load {
                code: -1,
                message: r,
            });
        }
        for (stage, wanted, fail) in [
            (StageId::DENOISE, denoise, cfg.fail_denoise_load),
            (StageId::RELIGHT, relight, cfg.fail_relight_load),
        ] {
            if wanted && let Some(r) = fail {
                if !reused {
                    self.loaded = None;
                    self.denoise = false;
                    self.relight = false;
                } else {
                    // The previous chain stays, less the stage (the seam:
                    // without it if it was not in it, the same).
                    if stage == StageId::DENOISE {
                        self.denoise = false;
                    } else {
                        self.relight = false;
                    }
                }
                return Err(BackendError::Stage {
                    stage,
                    code: -3,
                    message: r,
                });
            }
        }
        if reused
            && relight
            && self.relight
            && self.light != light
            && let Some(r) = cfg.fail_relight_rebind
        {
            self.relight = false;
            let hook = self.config.lock().unwrap().on_rebind_failure.take();
            if let Some(hook) = hook {
                (hook.0)();
            }
            return Err(BackendError::Stage {
                stage: StageId::RELIGHT,
                code: -6,
                message: r,
            });
        }
        match (req.effect, &req.background) {
            (EffectKind::None, _) if req.stages.is_empty() => {
                return Err(BackendError::InvalidInput(
                    "load(None) without a stage".into(),
                ));
            }
            (EffectKind::Replace, None) => {
                return Err(BackendError::InvalidInput(
                    "replace without background".into(),
                ));
            }
            (EffectKind::Replace, Some(bg)) => {
                bg.validate()?;
                if (bg.width, bg.height) != (req.width, req.height) {
                    return Err(BackendError::InvalidInput(format!(
                        "background {}x{} for frames {}x{}",
                        bg.width, bg.height, req.width, req.height
                    )));
                }
                self.log
                    .backgrounds
                    .lock()
                    .unwrap()
                    .push((bg.width, bg.height));
            }
            _ => {}
        }
        if relight && !(reused && self.relight) {
            self.relight_build = self.log.relight_builds.fetch_add(1, Ordering::SeqCst) + 1;
        } else if relight && self.light != light {
            self.log.relight_rebinds.fetch_add(1, Ordering::SeqCst);
        }
        if denoise && !(reused && self.denoise) {
            self.log.denoise_builds.fetch_add(1, Ordering::SeqCst);
        }
        self.loaded = Some(req.effect);
        self.denoise = denoise;
        self.relight = relight;
        self.light = light;
        self.stream = gpu.map_or(0, |g| g.stream as usize);
        self.context = gpu.map_or(0, |g| g.context as usize);
        self.host = gpu.is_none();
        self.log.loads.fetch_add(1, Ordering::SeqCst);
        if self.host {
            self.log.host_loads.fetch_add(1, Ordering::SeqCst);
        }
        self.log.loaded.lock().unwrap().push(req.effect);
        self.log
            .requests
            .lock()
            .unwrap()
            .push((req.effect, denoise));
        self.log.stage_requests.lock().unwrap().push((
            req.effect,
            req.stages.as_slice().to_vec(),
            light,
        ));
        let ms = t0.elapsed().as_secs_f64() * 1e3;
        Ok(LoadReport {
            load_ms: ms,
            warmup_ms: Some(0.0),
            reused_model: reused,
        })
    }

    /// The luma and chroma of a frame of the loaded chain, after the test's
    /// hooks and failures (shared by both memories).
    fn frame_output(&mut self) -> Result<(u8, u8), BackendError> {
        let hook = self.config.lock().unwrap().on_process.take();
        if let Some(hook) = hook {
            (hook.0)();
        }
        let lose = self.config.lock().unwrap().lose_chain_on_process.take();
        if let Some(r) = lose
            && self.loaded.is_some()
        {
            self.unload();
            self.log.lost_chains.fetch_add(1, Ordering::SeqCst);
            return Err(BackendError::Run {
                code: -6,
                message: r,
            });
        }
        let cfg = self.config.lock().unwrap().clone();
        if self.denoise
            && self.loaded.is_some()
            && let Some(r) = cfg.fail_denoise_process
        {
            // The stage is dropped; the next call runs without it.
            self.denoise = false;
            return Err(BackendError::Stage {
                stage: StageId::DENOISE,
                code: -4,
                message: r,
            });
        }
        if self.relight
            && self.loaded.is_some()
            && let Some(r) = cfg.fail_relight_process
        {
            self.relight = false;
            return Err(BackendError::Stage {
                stage: StageId::RELIGHT,
                code: -5,
                message: r,
            });
        }
        if let Some(r) = cfg.fail_process {
            return Err(BackendError::Run {
                code: -2,
                message: r,
            });
        }
        let y = match self.loaded {
            Some(EffectKind::Blur) => FAKE_BLUR_Y,
            Some(EffectKind::Replace) => FAKE_REPLACE_Y,
            Some(EffectKind::None) if self.relight => FAKE_RELIGHT_Y,
            Some(EffectKind::None) if self.denoise => FAKE_DENOISE_Y,
            _ => return Err(BackendError::InvalidInput("not loaded".into())),
        };
        let y = if self.relight && self.loaded != Some(EffectKind::None) {
            y + FAKE_RELIGHT_DY
        } else {
            y
        };
        let uv = if self.denoise { FAKE_DENOISE_UV } else { 128 };
        Ok((y, uv))
    }

    /// Counts a frame about to be written (shared by both memories).
    fn count_frame(&self, params: &FrameParams) {
        self.log.processes.fetch_add(1, Ordering::SeqCst);
        if self.denoise {
            self.log.denoised.fetch_add(1, Ordering::SeqCst);
        }
        if self.relight {
            self.log.relit.fetch_add(1, Ordering::SeqCst);
            self.log.relit_by.lock().unwrap().push(self.relight_build);
            let (pan, gain) = relight_frame(params);
            *self.log.last_relight_milli.lock().unwrap() = Some((
                (pan * 1000.0).round() as i64,
                (gain * 1000.0).round() as i64,
            ));
        }
        self.log.last_strength_milli.store(
            (params.blur_strength * 1000.0).round() as u64,
            Ordering::SeqCst,
        );
    }

    /// `reset_temporal` and `reset_temporal_host`.
    fn reset(&mut self) -> Result<StageList, BackendError> {
        self.log.resets.fetch_add(1, Ordering::SeqCst);
        let in_place = self.config.lock().unwrap().relight_resets_in_place;
        if self.relight && self.loaded.is_some() && !in_place {
            self.relight = false;
            self.log
                .relight_resets_dropped
                .fetch_add(1, Ordering::SeqCst);
            return Ok(StageId::RELIGHT.into());
        }
        Ok(StageList::default())
    }
}

impl InferenceBackend for FakeBackend {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn load(&mut self, gpu: &GpuContext, req: &LoadRequest) -> Result<LoadReport, BackendError> {
        self.check_context(gpu.context);
        self.load_on(Some(gpu), req)
    }

    fn load_host(&mut self, req: &LoadRequest) -> Result<LoadReport, BackendError> {
        self.load_on(None, req)
    }

    fn process(
        &mut self,
        gpu: &GpuContext,
        params: &FrameParams,
        src: &DeviceNv12,
        dst: &DeviceNv12,
    ) -> Result<(), BackendError> {
        self.check_context(gpu.context);
        self.check_memory(false);
        if gpu.stream as usize != self.stream {
            self.log.wrong_stream.fetch_add(1, Ordering::SeqCst);
        }
        let (y, uv) = self.frame_output()?;
        src.validate()?;
        dst.validate()?;
        self.count_frame(params);
        let (w, h) = (dst.width as usize, dst.height as usize);
        // SAFETY: `dst` is the caller's mapped output frame in the current
        // context; the stream is the caller's.
        unsafe {
            obc_gpu::memset_2d_async(dst.y, dst.y_pitch, y, w, h, gpu.stream)?;
            obc_gpu::memset_2d_async(dst.uv, dst.uv_pitch, uv, w, h / 2, gpu.stream)?;
        }
        if self.done.is_none() {
            self.done = Some(Event::new()?);
        }
        // SAFETY: the caller's stream, in the current (loaded) context, the
        // event's.
        unsafe {
            self.done
                .as_ref()
                .expect("created above")
                .record(gpu.stream)?
        };
        self.timed = (self.denoise, self.relight);
        self.host_done = false;
        Ok(())
    }

    /// The same frame as `process`, written on the CPU, except that the
    /// first Y row follows the input: each of its pixels is the input's
    /// plus one (saturating), so a test can tell the output was made from
    /// `src` (and that `src` and `dst` were not swapped). `src` is never
    /// written.
    fn process_host(
        &mut self,
        params: &FrameParams,
        src: &HostNv12,
        dst: &mut HostNv12,
    ) -> Result<(), BackendError> {
        self.check_memory(true);
        let (y, uv) = self.frame_output()?;
        src.validate()?;
        dst.validate()?;
        if (src.width, src.height) != (dst.width, dst.height) {
            return Err(BackendError::InvalidInput("src and dst differ".into()));
        }
        self.count_frame(params);
        self.log.host_processes.fetch_add(1, Ordering::SeqCst);
        let (w, h) = (dst.width as usize, dst.height as usize);
        let (yp, uvp) = (dst.y_pitch, dst.uv_pitch);
        // SAFETY: the caller's frame, as `HostNv12` documents it.
        let (y_plane, uv_plane) = unsafe { dst.planes_mut() }?;
        for r in 0..h {
            y_plane[r * yp..r * yp + w].fill(y);
        }
        for r in 0..h / 2 {
            uv_plane[r * uvp..r * uvp + w].fill(uv);
        }
        // SAFETY: `src` is the caller's readable frame of at least `w`
        // bytes per Y row (validated above), and does not overlap `dst`.
        let src_row = unsafe { std::slice::from_raw_parts(src.y.cast_const(), w) };
        for (d, s) in y_plane[..w].iter_mut().zip(src_row) {
            *d = s.saturating_add(1);
        }
        self.timed = (self.denoise, self.relight);
        self.host_done = true;
        Ok(())
    }

    /// Whether a chain is loaded: not after [`FakeConfig::lose_chain_on_process`].
    fn is_loaded(&self) -> bool {
        self.loaded.is_some()
    }

    fn unload(&mut self) {
        self.denoise = false;
        self.relight = false;
        self.timed = (false, false);
        self.host_done = false;
        // Destroyed with the context current (the caller's rule for unload).
        self.done = None;
        if self.loaded.take().is_some() {
            // An instance of host frames has no context to check.
            if !self.host {
                self.check_context(self.context as *mut c_void);
            }
            self.log.unloads.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Only the stages are "timed" (constants), so the element's stage
    /// windows can be checked; and only once the last frame's work
    /// has completed, as the trait says (a real backend reads its CUDA
    /// events): before the caller synchronises the stream, the frame is
    /// most likely still queued and there is nothing to report.
    fn timings(&self) -> Option<StageTimings> {
        let complete = self.host_done
            || self
                .done
                .as_ref()
                .is_some_and(|e| e.is_complete().unwrap_or(false));
        let (denoise, relight) = self.timed;
        (self.loaded.is_some() && (denoise || relight) && complete).then(|| {
            let mut t = StageTimings::new();
            if denoise {
                t.push("denoise", FAKE_DENOISE_MS);
            }
            if relight {
                t.push("relight", FAKE_RELIGHT_MS);
            }
            t
        })
    }

    fn status_values(&self) -> Vec<StageValue> {
        let Some(kind) = self.loaded else {
            return Vec::new();
        };
        let value = |stage: StageId, name: &str, value: f64| StageValue {
            stage,
            name: name.into(),
            value,
        };
        let mut out = Vec::new();
        if self.denoise {
            out.push(value(StageId::DENOISE, "level", FAKE_DENOISE_LEVEL));
        }
        if kind != EffectKind::None {
            out.push(value(StageId::BACKGROUND, "coverage", FAKE_COVERAGE));
        }
        // A stage that is not loaded: never reported by the element.
        out.push(value(FAKE_STRAY_STAGE, "stray", 1.0));
        out
    }

    fn reset_temporal(&mut self, gpu: &GpuContext) -> Result<StageList, BackendError> {
        self.check_context(gpu.context);
        self.check_memory(false);
        self.reset()
    }

    fn reset_temporal_host(&mut self) -> Result<StageList, BackendError> {
        self.check_memory(true);
        self.reset()
    }
}

impl Drop for FakeBackend {
    fn drop(&mut self) {
        if self.context != 0 {
            self.check_context(self.context as *mut c_void);
        }
        self.log.drops.fetch_add(1, Ordering::SeqCst);
    }
}
