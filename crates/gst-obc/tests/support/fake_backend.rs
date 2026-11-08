//! A fake inference backend for the effect tests: no models, a
//! configurable load delay and failures, and an output that is easy to
//! recognise: `blur` writes Y = [`FAKE_BLUR_Y`], `replace` Y =
//! [`FAKE_REPLACE_Y`], both with neutral chroma, through `obc-gpu` on the
//! stream it is given. It records every call and whether the caller kept
//! the context and stream rules of `obc_backend::InferenceBackend`.
//!
//! Shared by the GPU tests of `gst-obc` and `obc-pipeline` (included with
//! `#[path]`).

#![allow(dead_code)] // each including test uses a different subset

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use obc_backend::{
    BackendError, BackendFactory, DeviceNv12, EffectKind, FrameParams, GpuContext,
    InferenceBackend, LoadReport, LoadRequest, StageTimings,
};

/// The luma the fake blur writes.
pub const FAKE_BLUR_Y: u8 = 200;
/// The luma the fake replace writes.
pub const FAKE_REPLACE_Y: u8 = 100;

/// How the fake behaves; changeable while it runs.
#[derive(Debug, Clone, Default)]
pub struct FakeConfig {
    /// Sleep of a load on an unloaded instance (a cold model load).
    pub load_delay: Duration,
    /// Sleep of a load on a loaded instance (a switch between effects).
    pub switch_delay: Duration,
    /// `probe` and `create` fail with this reason (backend unavailable).
    pub fail_create: Option<String>,
    /// `load` fails with this reason.
    pub fail_load: Option<String>,
    /// `process` fails with this reason.
    pub fail_process: Option<String>,
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
            stream: 0,
            context: 0,
        }))
    }
}

pub struct FakeBackend {
    config: Arc<Mutex<FakeConfig>>,
    log: Arc<FakeLog>,
    loaded: Option<EffectKind>,
    /// The stream and context of the last load (as addresses: Send).
    stream: usize,
    context: usize,
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
        std::thread::sleep(if reused {
            cfg.switch_delay
        } else {
            cfg.load_delay
        });
        if let Some(r) = cfg.fail_load {
            if !reused {
                self.loaded = None;
            }
            return Err(BackendError::Load {
                code: -1,
                message: r,
            });
        }
        match (req.effect, &req.background) {
            (EffectKind::None, _) => {
                return Err(BackendError::InvalidInput("load(None)".into()));
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
        self.stream = gpu.stream as usize;
        self.context = gpu.context as usize;
        self.log.loads.fetch_add(1, Ordering::SeqCst);
        self.log.loaded.lock().unwrap().push(req.effect);
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
        if let Some(r) = self.config.lock().unwrap().fail_process.clone() {
            return Err(BackendError::Run {
                code: -2,
                message: r,
            });
        }
        let y = match self.loaded {
            Some(EffectKind::Blur) => FAKE_BLUR_Y,
            Some(EffectKind::Replace) => FAKE_REPLACE_Y,
            _ => return Err(BackendError::InvalidInput("not loaded".into())),
        };
        src.validate()?;
        dst.validate()?;
        self.log.processes.fetch_add(1, Ordering::SeqCst);
        self.log.last_strength_milli.store(
            (params.blur_strength * 1000.0).round() as u64,
            Ordering::SeqCst,
        );
        let (w, h) = (dst.width as usize, dst.height as usize);
        // SAFETY: `dst` is the caller's mapped output frame in the current
        // context; the stream is the caller's.
        unsafe {
            obc_gpu::memset_2d_async(dst.y, dst.y_pitch, y, w, h, gpu.stream)?;
            obc_gpu::memset_2d_async(dst.uv, dst.uv_pitch, 128, w, h / 2, gpu.stream)?;
        }
        Ok(())
    }

    fn unload(&mut self) {
        if self.loaded.take().is_some() {
            self.check_context(self.context as *mut c_void);
            self.log.unloads.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn timings(&self) -> Option<StageTimings> {
        None
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
