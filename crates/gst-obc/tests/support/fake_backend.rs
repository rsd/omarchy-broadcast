//! A fake inference backend for the effect tests: no models, a
//! configurable load delay and failures, and an output that is easy to
//! recognise: `blur` writes Y = [`FAKE_BLUR_Y`], `replace` Y =
//! [`FAKE_REPLACE_Y`], the denoise stage alone Y = [`FAKE_DENOISE_Y`], all
//! with neutral chroma unless the denoise stage ran, which writes chroma
//! [`FAKE_DENOISE_UV`] instead, through `obc-gpu` on the stream it is given.
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
    InferenceBackend, LoadReport, LoadRequest, StageTimings,
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
    /// A `load` with `denoise` fails with `BackendError::Denoise` and this
    /// reason (nothing of the request in force, as the seam says).
    pub fail_denoise_load: Option<String>,
    /// A `process` with the denoise stage loaded fails with
    /// `BackendError::Denoise` and this reason, and drops the stage.
    pub fail_denoise_process: Option<String>,
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
            stream: 0,
            context: 0,
            done: None,
            timed: false,
        }))
    }
}

pub struct FakeBackend {
    config: Arc<Mutex<FakeConfig>>,
    log: Arc<FakeLog>,
    loaded: Option<EffectKind>,
    /// The denoise stage is loaded (with `loaded`).
    denoise: bool,
    /// The stream and context of the last load (as addresses: Send).
    stream: usize,
    context: usize,
    /// Recorded on the stream after each frame's work (created with the
    /// first frame, in the loaded context; dropped on unload).
    done: Option<Event>,
    /// The denoise stage ran on the last frame.
    timed: bool,
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
            }
            return Err(BackendError::Load {
                code: -1,
                message: r,
            });
        }
        if req.denoise
            && let Some(r) = cfg.fail_denoise_load
        {
            if !reused {
                self.loaded = None;
                self.denoise = false;
            }
            return Err(BackendError::Denoise {
                code: -3,
                message: r,
            });
        }
        match (req.effect, &req.background) {
            (EffectKind::None, _) if !req.denoise => {
                return Err(BackendError::InvalidInput(
                    "load(None) without denoise".into(),
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
        self.loaded = Some(req.effect);
        self.denoise = req.denoise;
        self.stream = gpu.stream as usize;
        self.context = gpu.context as usize;
        self.log.loads.fetch_add(1, Ordering::SeqCst);
        self.log.loaded.lock().unwrap().push(req.effect);
        self.log
            .requests
            .lock()
            .unwrap()
            .push((req.effect, req.denoise));
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
            return Err(BackendError::Denoise {
                code: -4,
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
            Some(EffectKind::None) if self.denoise => FAKE_DENOISE_Y,
            _ => return Err(BackendError::InvalidInput("not loaded".into())),
        };
        let uv = if self.denoise { FAKE_DENOISE_UV } else { 128 };
        src.validate()?;
        dst.validate()?;
        self.log.processes.fetch_add(1, Ordering::SeqCst);
        if self.denoise {
            self.log.denoised.fetch_add(1, Ordering::SeqCst);
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
        self.timed = self.denoise;
        Ok(())
    }

    fn unload(&mut self) {
        self.denoise = false;
        self.timed = false;
        // Destroyed with the context current (the caller's rule for unload).
        self.done = None;
        if self.loaded.take().is_some() {
            self.check_context(self.context as *mut c_void);
            self.log.unloads.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Only the denoise stage is "timed" (a constant), so the element's
    /// denoise window can be checked; and only once the last frame's work
    /// has completed, as the trait says (a real backend reads its CUDA
    /// events): before the caller synchronises the stream, the frame is
    /// most likely still queued and there is nothing to report.
    fn timings(&self) -> Option<StageTimings> {
        let complete = self
            .done
            .as_ref()
            .is_some_and(|e| e.is_complete().unwrap_or(false));
        (self.loaded.is_some() && self.timed && complete).then(|| StageTimings {
            denoise_ms: FAKE_DENOISE_MS,
            ..StageTimings::default()
        })
    }

    fn reset_temporal(&mut self, gpu: &GpuContext) -> Result<(), BackendError> {
        self.check_context(gpu.context);
        self.log.resets.fetch_add(1, Ordering::SeqCst);
        Ok(())
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
