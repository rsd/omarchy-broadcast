//! A fake module chain for the `obcmodule` tests: no models, configurable
//! failures, and an output that is easy to recognise. Its stages are
//!
//! * `tint`: writes every chroma byte (U and V) with the live parameter `u`
//!   (an int, default [`FAKE_TINT_UV`]);
//! * `lift`: writes every luma byte with the load-time parameter `y` (an
//!   int, default [`FAKE_LIFT_Y`]);
//! * `bump`: adds [`FAKE_BUMP_Y`] to every luma byte (wrapping): unlike the
//!   others it is not idempotent, so a frame it ran on twice shows;
//! * `still`: does nothing to the pixels; its state cannot be reset in
//!   place, so `reset_temporal` drops it (as a stage that would carry the
//!   previous source otherwise).
//!
//! While `lift` is loaded, the chain reports the status value `lift`/`y`,
//! its luma, and while `tint` is, `tint`/`frames`, the frames since the
//! load.
//!
//! It runs on host frames ([`FakeChainFactory::host`]) or on CUDA frames
//! ([`FakeChainFactory::cuda`], written with `obc-gpu` memsets on the stream
//! of the load), and records every call.
//!
//! Shared by the tests of `gst-obc` and `obc-pipeline` (included with
//! `#[path]`).

#![allow(dead_code)] // each including test uses a different subset

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use obc_backend::{
    BackendError, ChainFrame, ChainRequest, FrameKind, LoadReport, Params, StageChain,
    StageChainFactory, StageId, StageList, StageValue,
};

/// The chroma `tint` writes without a live `u`.
pub const FAKE_TINT_UV: u8 = 90;
/// The luma `lift` writes without a load-time `y`.
pub const FAKE_LIFT_Y: u8 = 200;
/// What `bump` adds to every luma byte.
pub const FAKE_BUMP_Y: u8 = 10;
/// The byte a failing call writes over the chroma with
/// [`FakeChainConfig::scribble_on_fail`].
pub const FAKE_SCRIBBLE: u8 = 0xEE;

/// How the fake behaves; changeable while it runs.
#[derive(Debug, Clone, Default)]
pub struct FakeChainConfig {
    /// Sleep of every load.
    pub load_delay: Duration,
    /// Every load fails as a whole with this reason.
    pub fail_load: Option<String>,
    /// A load with this stage fails for the stage alone.
    pub fail_load_stage: Option<String>,
    /// A frame with this stage fails for the stage alone (once per load of
    /// the stage).
    pub fail_frame_stage: Option<String>,
    /// Every frame fails as a whole.
    pub fail_frame: bool,
    /// `create` fails with this reason.
    pub fail_create: Option<String>,
    /// A failing frame call (either failure above) first runs every stage
    /// loaded, the failing one included, and writes [`FAKE_SCRIBBLE`] over
    /// the chroma: it leaves a partly processed frame, as a real chain
    /// failing midway would.
    pub scribble_on_fail: bool,
}

/// The stages and load-time parameters of a load request.
pub type Request = (Vec<String>, BTreeMap<StageId, Params>);

/// What the fake saw.
#[derive(Debug, Default)]
pub struct FakeChainLog {
    pub creates: AtomicU64,
    pub loads: AtomicU64,
    pub frames: AtomicU64,
    pub resets: AtomicU64,
    /// The frames among `frames` that came as repeats
    /// (`StageChain::process_repeat`); the fake processes them as any frame.
    pub repeats: AtomicU64,
    pub unloads: AtomicU64,
    /// Calls with a frame of another kind or size than the load's, or a
    /// CUDA call without the load's context current.
    pub misuse: AtomicU64,
    /// The frame size of every load, in order.
    pub load_sizes: Mutex<Vec<(u32, u32)>>,
    /// The last load request: its stages and parameters.
    pub last_request: Mutex<Option<Request>>,
    /// The live parameters of the last frame.
    pub last_live: Mutex<BTreeMap<StageId, Params>>,
}

impl FakeChainLog {
    pub fn get(a: &AtomicU64) -> u64 {
        a.load(Ordering::SeqCst)
    }
}

pub struct FakeChainFactory {
    kinds: Vec<FrameKind>,
    pub config: Arc<Mutex<FakeChainConfig>>,
    pub log: Arc<FakeChainLog>,
}

impl FakeChainFactory {
    /// A fake whose chains take host frames only.
    pub fn host(config: FakeChainConfig) -> Arc<FakeChainFactory> {
        Self::with_kinds(vec![FrameKind::Host], config)
    }

    /// A fake whose chains take CUDA frames only.
    pub fn cuda(config: FakeChainConfig) -> Arc<FakeChainFactory> {
        Self::with_kinds(vec![FrameKind::Cuda], config)
    }

    pub fn with_kinds(kinds: Vec<FrameKind>, config: FakeChainConfig) -> Arc<FakeChainFactory> {
        Arc::new(FakeChainFactory {
            kinds,
            config: Arc::new(Mutex::new(config)),
            log: Arc::default(),
        })
    }

    pub fn set(&self, f: impl FnOnce(&mut FakeChainConfig)) {
        f(&mut self.config.lock().unwrap());
    }
}

impl StageChainFactory for FakeChainFactory {
    fn name(&self) -> &str {
        "fake"
    }

    fn kinds(&self) -> &[FrameKind] {
        &self.kinds
    }

    fn create(&self) -> Result<Box<dyn StageChain>, BackendError> {
        if let Some(r) = self.config.lock().unwrap().fail_create.clone() {
            return Err(BackendError::Unavailable(r));
        }
        self.log.creates.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(FakeChain {
            config: Arc::clone(&self.config),
            log: Arc::clone(&self.log),
            loaded: None,
            frame_failed: false,
        }))
    }
}

struct Loaded {
    stages: Vec<StageId>,
    lift_y: u8,
    /// Frames processed since the load.
    frames: u64,
    kind: FrameKind,
    size: (u32, u32),
    context: *mut std::ffi::c_void,
}

struct FakeChain {
    config: Arc<Mutex<FakeChainConfig>>,
    log: Arc<FakeChainLog>,
    loaded: Option<Loaded>,
    /// `fail_frame_stage` fired since the stage was last loaded.
    frame_failed: bool,
}

// SAFETY: the context pointer is an opaque handle only compared.
unsafe impl Send for FakeChain {}

fn stage_error(stage: &str, message: &str) -> BackendError {
    BackendError::Stage {
        stage: StageId::new(stage).unwrap(),
        code: 7,
        message: message.into(),
    }
}

fn int(p: Option<&Params>, key: &str, default: u8) -> u8 {
    p.and_then(|p| p.get(key))
        .and_then(|v| v.as_int())
        .map_or(default, |v| v.clamp(0, 255) as u8)
}

impl StageChain for FakeChain {
    fn load(&mut self, req: &ChainRequest) -> Result<LoadReport, BackendError> {
        req.validate()?;
        let cfg = self.config.lock().unwrap().clone();
        std::thread::sleep(cfg.load_delay);
        if let Some(r) = cfg.fail_load {
            self.loaded = None;
            return Err(BackendError::Load {
                code: 3,
                message: r,
            });
        }
        if let Some(s) = &cfg.fail_load_stage
            && req.stages.contains(s)
        {
            if let Some(l) = self.loaded.as_mut() {
                l.stages.retain(|t| t != s.as_str());
            }
            return Err(stage_error(s, &format!("fake: {s} cannot load")));
        }
        if req.kind == FrameKind::Cuda
            && obc_gpu::current_context().ok() != req.gpu.map(|g| g.context)
        {
            self.log.misuse.fetch_add(1, Ordering::SeqCst);
        }
        let had = self
            .loaded
            .as_ref()
            .map(|l| l.stages.clone())
            .unwrap_or_default();
        self.loaded = Some(Loaded {
            stages: req.stages.as_slice().to_vec(),
            lift_y: int(req.params.get("lift"), "y", FAKE_LIFT_Y),
            frames: 0,
            kind: req.kind,
            size: (req.width, req.height),
            context: req.gpu.map_or(std::ptr::null_mut(), |g| g.context),
        });
        self.frame_failed = false;
        self.log.loads.fetch_add(1, Ordering::SeqCst);
        self.log
            .load_sizes
            .lock()
            .unwrap()
            .push((req.width, req.height));
        *self.log.last_request.lock().unwrap() = Some((
            req.stages
                .as_slice()
                .iter()
                .map(|s| s.to_string())
                .collect(),
            req.params.clone(),
        ));
        Ok(LoadReport {
            load_ms: cfg.load_delay.as_secs_f64() * 1000.0,
            warmup_ms: None,
            reused_model: !had.is_empty(),
        })
    }

    fn process(
        &mut self,
        frame: ChainFrame<'_>,
        live: &BTreeMap<StageId, Params>,
    ) -> Result<(), BackendError> {
        let cfg = self.config.lock().unwrap().clone();
        let Some(l) = self.loaded.as_mut() else {
            return Err(BackendError::InvalidInput("fake: nothing loaded".into()));
        };
        if frame.kind() != l.kind || frame.size() != l.size {
            self.log.misuse.fetch_add(1, Ordering::SeqCst);
            return Err(BackendError::InvalidInput("fake: wrong frame".into()));
        }
        let lift = l.stages.iter().any(|s| s == "lift").then_some(l.lift_y);
        let bump = l.stages.iter().any(|s| s == "bump");
        let mut failure = None;
        if cfg.fail_frame {
            failure = Some(BackendError::Run {
                code: 9,
                message: "fake: the frame failed".into(),
            });
        } else if let Some(s) = &cfg.fail_frame_stage
            && !self.frame_failed
            && l.stages.iter().any(|t| t == s.as_str())
        {
            failure = Some(stage_error(s, &format!("fake: {s} failed on a frame")));
        }
        let tint = l
            .stages
            .iter()
            .any(|s| s == "tint")
            .then(|| int(live.get("tint"), "u", FAKE_TINT_UV));
        if let Some(BackendError::Stage { stage, .. }) = &failure {
            l.stages.retain(|t| t != stage);
            self.frame_failed = true;
        }
        if let Some(err) = failure.clone()
            && !cfg.scribble_on_fail
        {
            return Err(err);
        }
        if failure.is_none() {
            self.log.frames.fetch_add(1, Ordering::SeqCst);
            *self.log.last_live.lock().unwrap() = live.clone();
            l.frames += 1;
        }
        let tint = if failure.is_some() {
            Some(FAKE_SCRIBBLE)
        } else {
            tint
        };
        match frame {
            ChainFrame::Host(f) => {
                let (w, h) = (f.width as usize, f.height as usize);
                let (yp, uvp) = (f.y_pitch, f.uv_pitch);
                // SAFETY: the caller's contract of a host frame.
                let (y, uv) = unsafe { f.planes_mut() }?;
                if let Some(v) = lift {
                    for r in 0..h {
                        y[r * yp..r * yp + w].fill(v);
                    }
                }
                if bump {
                    for r in 0..h {
                        for b in &mut y[r * yp..r * yp + w] {
                            *b = b.wrapping_add(FAKE_BUMP_Y);
                        }
                    }
                }
                if let Some(v) = tint {
                    for r in 0..h / 2 {
                        uv[r * uvp..r * uvp + w].fill(v);
                    }
                }
            }
            ChainFrame::Cuda { gpu, frame } => {
                if obc_gpu::current_context().ok() != Some(l.context) || gpu.context != l.context {
                    self.log.misuse.fetch_add(1, Ordering::SeqCst);
                }
                let (w, h) = (frame.width as usize, frame.height as usize);
                // SAFETY: the caller's frame, in the current context, on the
                // stream of the load.
                unsafe {
                    if let Some(v) = lift {
                        obc_gpu::memset_2d_async(frame.y, frame.y_pitch, v, w, h, gpu.stream)?;
                    }
                    if bump {
                        // Through the host: a fake needs no kernel.
                        obc_gpu::stream_synchronize(gpu.stream)?;
                        let mut y = vec![0u8; w * h];
                        obc_gpu::download_2d(frame.y, frame.y_pitch, &mut y, w, w, h)?;
                        for b in &mut y {
                            *b = b.wrapping_add(FAKE_BUMP_Y);
                        }
                        obc_gpu::upload_2d(&y, w, frame.y, frame.y_pitch, w, h)?;
                    }
                    if let Some(v) = tint {
                        obc_gpu::memset_2d_async(
                            frame.uv,
                            frame.uv_pitch,
                            v,
                            w,
                            h / 2,
                            gpu.stream,
                        )?;
                    }
                }
            }
        }
        match failure {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn process_repeat(
        &mut self,
        frame: ChainFrame<'_>,
        live: &BTreeMap<StageId, Params>,
    ) -> Result<(), BackendError> {
        self.log.repeats.fetch_add(1, Ordering::SeqCst);
        self.process(frame, live)
    }

    fn reset_temporal(&mut self) -> Result<StageList, BackendError> {
        self.log.resets.fetch_add(1, Ordering::SeqCst);
        let Some(l) = self.loaded.as_mut() else {
            return Ok(StageList::default());
        };
        let dropped: Vec<StageId> = l.stages.iter().filter(|s| *s == "still").cloned().collect();
        l.stages.retain(|s| s != "still");
        StageList::new(dropped)
    }

    fn unload(&mut self) {
        if self.loaded.take().is_some() {
            self.log.unloads.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn status_values(&self) -> Vec<StageValue> {
        let Some(l) = &self.loaded else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for s in &l.stages {
            match s.as_str() {
                "lift" => out.push(StageValue {
                    stage: s.clone(),
                    name: "y".into(),
                    value: f64::from(l.lift_y),
                }),
                "tint" => out.push(StageValue {
                    stage: s.clone(),
                    name: "frames".into(),
                    value: l.frames as f64,
                }),
                _ => {}
            }
        }
        out
    }
}
