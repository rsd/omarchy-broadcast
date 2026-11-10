//! A fake inference backend for the effect tests: no models, a
//! configurable load delay and failures, and an output that is easy to
//! recognise: `blur` writes Y = [`FAKE_BLUR_Y`], `replace` Y =
//! [`FAKE_REPLACE_Y`], the denoise stage alone Y = [`FAKE_DENOISE_Y`], the
//! relight stage alone (with or without denoise) Y = [`FAKE_RELIGHT_Y`],
//! and the relight stage under an effect adds [`FAKE_RELIGHT_DY`] to the
//! effect's Y; all with neutral chroma unless the denoise stage ran, which
//! writes chroma [`FAKE_DENOISE_UV`] instead, through `obc-gpu` on the
//! stream it is given.
//! Like Maxine, its `reset_temporal` drops a loaded relight stage and returns
//! it (unless `FakeConfig::relight_resets_in_place`); each build of the
//! stage is numbered, so a test can tell which one relit a frame.
//! It records every call and whether the caller kept the context and stream
//! rules of `obc_backend::InferenceBackend`. Like a real backend, it reports
//! a frame's stage timings (`timings`) only once that frame's work has
//! completed on the GPU (a CUDA event recorded after it), so a caller that
//! reads them before synchronising the stream gets none.
//!
//! Shared by the GPU tests of `gst-obc` and `obc-pipeline` (included with
//! `#[path]`).

#![allow(dead_code)] // each including test uses a different subset

use std::ffi::c_void;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use obc_backend::{
    BackendError, BackendFactory, DeviceNv12, EffectKind, FrameParams, GpuContext,
    InferenceBackend, LoadReport, LoadRequest, RelightLight, Stage, StageList, StageTimings,
};
use obc_gpu::Event;

/// The luma the fake blur writes.
pub const FAKE_BLUR_Y: u8 = 200;
/// The luma the fake replace writes.
pub const FAKE_REPLACE_Y: u8 = 100;
/// The luma the fake denoise stage writes when it runs alone.
pub const FAKE_DENOISE_Y: u8 = 60;
/// The chroma (both U and V) the fake writes when the denoise stage ran.
pub const FAKE_DENOISE_UV: u8 = 90;
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
    pub stage_requests: Mutex<Vec<(EffectKind, Vec<Stage>, RelightLight)>>,
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
}

impl FakeFactory {
    pub fn new(config: FakeConfig) -> Arc<FakeFactory> {
        Arc::new(FakeFactory {
            config: Arc::new(Mutex::new(config)),
            log: Arc::default(),
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
}

impl FakeBackend {
    fn check_context(&self, expected: *mut c_void) {
        let current = obc_gpu::current_context().unwrap_or(std::ptr::null_mut());
        if current.is_null() || current != expected {
            self.log.wrong_context.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl InferenceBackend for FakeBackend {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn load(&mut self, gpu: &GpuContext, req: &LoadRequest) -> Result<LoadReport, BackendError> {
        self.check_context(gpu.context);
        let denoise = req.stages.contains(Stage::Denoise);
        let relight = req.stages.contains(Stage::Relight);
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
            (Stage::Denoise, denoise, cfg.fail_denoise_load),
            (Stage::Relight, relight, cfg.fail_relight_load),
        ] {
            if wanted && let Some(r) = fail {
                if !reused {
                    self.loaded = None;
                    self.denoise = false;
                    self.relight = false;
                } else {
                    // The previous chain stays, less the stage (the seam:
                    // without it if it was not in it, the same).
                    match stage {
                        Stage::Denoise => self.denoise = false,
                        Stage::Relight => self.relight = false,
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
            && self.light != req.relight_light
            && let Some(r) = cfg.fail_relight_rebind
        {
            self.relight = false;
            let hook = self.config.lock().unwrap().on_rebind_failure.take();
            if let Some(hook) = hook {
                (hook.0)();
            }
            return Err(BackendError::Stage {
                stage: Stage::Relight,
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
        } else if relight && self.light != req.relight_light {
            self.log.relight_rebinds.fetch_add(1, Ordering::SeqCst);
        }
        if denoise && !(reused && self.denoise) {
            self.log.denoise_builds.fetch_add(1, Ordering::SeqCst);
        }
        self.loaded = Some(req.effect);
        self.denoise = denoise;
        self.relight = relight;
        self.light = req.relight_light;
        self.stream = gpu.stream as usize;
        self.context = gpu.context as usize;
        self.log.loads.fetch_add(1, Ordering::SeqCst);
        self.log.loaded.lock().unwrap().push(req.effect);
        self.log
            .requests
            .lock()
            .unwrap()
            .push((req.effect, denoise));
        self.log.stage_requests.lock().unwrap().push((
            req.effect,
            req.stages.as_slice().to_vec(),
            req.relight_light,
        ));
        let ms = t0.elapsed().as_secs_f64() * 1e3;
        Ok(LoadReport {
            load_ms: ms,
            warmup_ms: Some(0.0),
            reused_model: reused,
        })
    }

    fn process(
        &mut self,
        gpu: &GpuContext,
        params: &FrameParams,
        src: &DeviceNv12,
        dst: &DeviceNv12,
    ) -> Result<(), BackendError> {
        self.check_context(gpu.context);
        if gpu.stream as usize != self.stream {
            self.log.wrong_stream.fetch_add(1, Ordering::SeqCst);
        }
        let hook = self.config.lock().unwrap().on_process.take();
        if let Some(hook) = hook {
            (hook.0)();
        }
        let cfg = self.config.lock().unwrap().clone();
        if self.denoise
            && self.loaded.is_some()
            && let Some(r) = cfg.fail_denoise_process
        {
            // The stage is dropped; the next call runs without it.
            self.denoise = false;
            return Err(BackendError::Stage {
                stage: Stage::Denoise,
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
                stage: Stage::Relight,
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
        src.validate()?;
        dst.validate()?;
        self.log.processes.fetch_add(1, Ordering::SeqCst);
        if self.denoise {
            self.log.denoised.fetch_add(1, Ordering::SeqCst);
        }
        if self.relight {
            self.log.relit.fetch_add(1, Ordering::SeqCst);
            self.log.relit_by.lock().unwrap().push(self.relight_build);
            let (pan, gain) = params.relight();
            *self.log.last_relight_milli.lock().unwrap() = Some((
                (pan * 1000.0).round() as i64,
                (gain * 1000.0).round() as i64,
            ));
        }
        self.log.last_strength_milli.store(
            (params.blur_strength * 1000.0).round() as u64,
            Ordering::SeqCst,
        );
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
        Ok(())
    }

    fn unload(&mut self) {
        self.denoise = false;
        self.relight = false;
        self.timed = (false, false);
        // Destroyed with the context current (the caller's rule for unload).
        self.done = None;
        if self.loaded.take().is_some() {
            self.check_context(self.context as *mut c_void);
            self.log.unloads.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Only the stages are "timed" (constants), so the element's stage
    /// windows can be checked; and only once the last frame's work
    /// has completed, as the trait says (a real backend reads its CUDA
    /// events): before the caller synchronises the stream, the frame is
    /// most likely still queued and there is nothing to report.
    fn timings(&self) -> Option<StageTimings> {
        let complete = self
            .done
            .as_ref()
            .is_some_and(|e| e.is_complete().unwrap_or(false));
        let (denoise, relight) = self.timed;
        (self.loaded.is_some() && (denoise || relight) && complete).then(|| StageTimings {
            denoise_ms: if denoise { FAKE_DENOISE_MS } else { 0.0 },
            relight_ms: if relight { FAKE_RELIGHT_MS } else { 0.0 },
            ..StageTimings::default()
        })
    }

    fn reset_temporal(&mut self, gpu: &GpuContext) -> Result<StageList, BackendError> {
        self.check_context(gpu.context);
        self.log.resets.fetch_add(1, Ordering::SeqCst);
        let in_place = self.config.lock().unwrap().relight_resets_in_place;
        if self.relight && self.loaded.is_some() && !in_place {
            self.relight = false;
            self.log
                .relight_resets_dropped
                .fetch_add(1, Ordering::SeqCst);
            return Ok(Stage::Relight.into());
        }
        Ok(StageList::default())
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
