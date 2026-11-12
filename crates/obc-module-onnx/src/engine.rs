//! The engines a model can run on: ONNX Runtime's execution providers.
//!
//! `auto` tries, in order, TensorRT, CUDA, OpenVINO on the NPU, on the GPU
//! and on the CPU, then ONNX Runtime's own CPU provider, and takes the
//! first that starts. The probe of each is generic: it asks the loaded
//! runtime which providers it was built with (`GetAvailableProviders`),
//! then tries to create a session with the provider registered and to run
//! it once. A fuller build of ONNX Runtime (with TensorRT and OpenVINO)
//! lights the other engines up with no change here.
//!
//! **Partly on the CPU.** ONNX Runtime places a node its provider cannot
//! run on the CPU, silently. The first run of a session is profiled
//! (`EnableProfiling`, then `EndProfiling` after the run, which also stops
//! profiling for the runs after it) and the profile tells which provider
//! ran each kernel: an engine that ran no kernel of the model did not
//! start (OpenVINO falls back like this when its device is missing), and
//! one that left some to the CPU is reported as "partly on the CPU".
//!
//! An engine chosen explicitly that cannot run fails the load with its
//! reason; only `auto` moves on to the next.

use std::path::{Path, PathBuf};
use std::time::Instant;

use obc_module_sdk::log;
use obc_module_sdk::serde_json;
use ort::ep::{self, ExecutionProviderDispatch};
use ort::session::Session;
use ort::session::builder::{GraphOptimizationLevel, SessionBuilder};
use ort::value::TensorRef;

use crate::catalogue::ModelSpec;
use crate::runtime::{Runtime, take_warnings};

/// The engine setting's values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Engine {
    /// The first that starts, in [`AUTO_ORDER`].
    Auto,
    /// NVIDIA TensorRT (fp16, with an engine cache).
    TensorRt,
    /// NVIDIA CUDA.
    Cuda,
    /// OpenVINO on an Intel NPU.
    OpenVinoNpu,
    /// OpenVINO on a GPU.
    OpenVinoGpu,
    /// OpenVINO on the CPU.
    OpenVinoCpu,
    /// ONNX Runtime's own CPU provider.
    Cpu,
}

/// The order `auto` tries the engines in.
pub const AUTO_ORDER: [Engine; 6] = [
    Engine::TensorRt,
    Engine::Cuda,
    Engine::OpenVinoNpu,
    Engine::OpenVinoGpu,
    Engine::OpenVinoCpu,
    Engine::Cpu,
];

/// ONNX Runtime's name of its CPU provider.
pub const CPU_PROVIDER: &str = "CPUExecutionProvider";

impl Engine {
    /// Every value, in the settings' order.
    pub const ALL: [Engine; 7] = [
        Engine::Auto,
        Engine::TensorRt,
        Engine::Cuda,
        Engine::OpenVinoNpu,
        Engine::OpenVinoGpu,
        Engine::OpenVinoCpu,
        Engine::Cpu,
    ];

    /// The setting's value.
    pub fn as_str(self) -> &'static str {
        match self {
            Engine::Auto => "auto",
            Engine::TensorRt => "tensorrt",
            Engine::Cuda => "cuda",
            Engine::OpenVinoNpu => "openvino-npu",
            Engine::OpenVinoGpu => "openvino-gpu",
            Engine::OpenVinoCpu => "openvino-cpu",
            Engine::Cpu => "cpu",
        }
    }

    /// The engine of a setting's value.
    pub fn parse(s: &str) -> Option<Engine> {
        Engine::ALL.into_iter().find(|e| e.as_str() == s)
    }

    /// The option's label in the settings, and the engine's name in
    /// reasons and logs.
    pub fn label(self) -> &'static str {
        match self {
            Engine::Auto => "Automatic",
            Engine::TensorRt => "NVIDIA TensorRT",
            Engine::Cuda => "NVIDIA CUDA",
            Engine::OpenVinoNpu => "OpenVINO NPU",
            Engine::OpenVinoGpu => "OpenVINO GPU",
            Engine::OpenVinoCpu => "OpenVINO on the CPU",
            Engine::Cpu => "CPU",
        }
    }

    /// The execution provider, as ONNX Runtime names it.
    pub fn provider(self) -> Option<&'static str> {
        match self {
            Engine::Auto => None,
            Engine::TensorRt => Some("TensorrtExecutionProvider"),
            Engine::Cuda => Some("CUDAExecutionProvider"),
            Engine::OpenVinoNpu | Engine::OpenVinoGpu | Engine::OpenVinoCpu => {
                Some("OpenVINOExecutionProvider")
            }
            Engine::Cpu => Some(CPU_PROVIDER),
        }
    }

    /// Whether the engine runs on an NVIDIA GPU through the CUDA runtime,
    /// which changes the calling thread's current CUDA context.
    pub fn uses_cuda(self) -> bool {
        matches!(self, Engine::TensorRt | Engine::Cuda)
    }

    /// The engines a request for `self` tries, in order.
    pub fn candidates(self) -> Vec<Engine> {
        match self {
            Engine::Auto => AUTO_ORDER.to_vec(),
            e => vec![e],
        }
    }
}

/// Where an engine's caches go.
#[derive(Debug, Clone)]
pub struct Caches {
    /// `$XDG_CACHE_HOME/omarchy-broadcast` (or `~/.cache/...`); none when
    /// neither is known, and then no cache is kept.
    pub root: Option<PathBuf>,
}

impl Caches {
    /// The cache root from the environment `env`.
    pub fn from_env(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Caches {
        let set = |n: &str| env(n).filter(|v| !v.is_empty()).map(PathBuf::from);
        let root = set("XDG_CACHE_HOME")
            .filter(|p| p.is_absolute())
            .or_else(|| set("HOME").map(|h| h.join(".cache")))
            .map(|c| c.join("omarchy-broadcast"));
        Caches { root }
    }

    /// The TensorRT engine cache of a model: `trt/<the first 16 hex digits
    /// of the model's SHA-256>-ort<ONNX Runtime version>-trt<TensorRT
    /// version>-<GPU name>-sm<compute capability>[-<h>x<w>]/`. The key
    /// covers the model file, the runtime (whose TensorRT provider writes
    /// the engines), TensorRT and the GPU (an engine runs only where it was
    /// built), and for a model of dynamic size the input size (an engine is
    /// built for one shape), so that an engine of another GPU or TensorRT
    /// is never taken for this one's. ONNX Runtime's own file names inside
    /// carry the compute capability and the precision.
    pub fn tensorrt_dir(
        &self,
        model: &ModelSpec,
        key: &TrtKey<'_>,
        input: (u32, u32),
    ) -> Option<PathBuf> {
        let trt = key.tensorrt.as_deref().unwrap_or("unknown");
        let device = match &key.device {
            Some((name, (major, minor))) => format!("{}-sm{major}{minor}", slug(name)),
            None => "unknown-gpu".to_string(),
        };
        let mut dir = format!("{}-ort{}-trt{trt}-{device}", &model.sha256[..16], key.ort);
        if matches!(model.geometry, crate::catalogue::Geometry::ShortSide { .. }) {
            dir.push_str(&format!("-{}x{}", input.0, input.1));
        }
        self.root.as_ref().map(|r| r.join("trt").join(dir))
    }

    /// OpenVINO's model cache directory.
    pub fn openvino_dir(&self) -> Option<PathBuf> {
        self.root.as_ref().map(|r| r.join("openvino"))
    }
}

/// What a TensorRT engine is built for, besides the model.
#[derive(Debug, Clone)]
pub struct TrtKey<'a> {
    /// ONNX Runtime's version.
    pub ort: &'a str,
    /// TensorRT's version (`10.16.1`), when it can be read.
    pub tensorrt: Option<String>,
    /// The GPU's name and compute capability.
    pub device: Option<(String, (i32, i32))>,
}

/// A name for a file name: lower-case letters and digits, the rest dashes.
fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// A started session.
pub struct Started {
    /// The session.
    pub session: Session,
    /// The engine it runs on.
    pub engine: Engine,
    /// The name of the model's image input.
    pub input_name: String,
    /// `Some((on the CPU, all))` kernels when the provider left some of
    /// the model to the CPU.
    pub partly_on_cpu: Option<(usize, usize)>,
    /// Whether the TensorRT engine was built (not read from the cache).
    pub built_tensorrt_engine: bool,
    /// The time to create the session, in milliseconds.
    pub load_ms: f32,
    /// The time of the first run (the warm-up), in milliseconds.
    pub warmup_ms: f32,
    /// The count of NVIDIA sessions this one is in. Declared after
    /// `session`, so dropped after it: the last one resets the primary
    /// context once its session is gone.
    pub _gpu: Option<crate::cuda::GpuSession>,
}

/// What a request for an engine needs.
pub struct Request<'a> {
    /// The runtime.
    pub runtime: &'a Runtime,
    /// The model.
    pub model: &'static ModelSpec,
    /// Its file's bytes, checked against the catalogue.
    pub bytes: &'a [u8],
    /// The model's input size, `(height, width)`.
    pub input: (u32, u32),
    /// The CPU threads of a session.
    pub threads: usize,
    /// The caches.
    pub caches: &'a Caches,
    /// Whether the load is a switch: a chain is running, and the stream
    /// waits for this load.
    pub switch: bool,
    /// Whether CUDA may be used at all: false when the host's configuration
    /// says the video pipeline runs in system memory. Then no engine that
    /// uses CUDA is tried and no CUDA call is made.
    pub cuda: bool,
}

/// Why the engines that did not start did not.
pub type Reasons = Vec<(Engine, String)>;

/// Starts the model on `wanted` (for `auto`, the first engine that
/// starts). Every engine tried that did not start is logged, one line
/// each, and returned with its reason; the error of an engine chosen
/// explicitly, or of `auto` when none starts, names them.
pub fn start(req: &Request<'_>, wanted: Engine) -> Result<(Started, Reasons), String> {
    let (started, reasons) = first_that_starts(
        wanted,
        req.cuda,
        req.model.id,
        || req.switch && tensorrt_would_build(req),
        |engine| try_engine(req, engine),
    )?;
    log_started(req, &started);
    Ok((started, reasons))
}

/// [`start`]'s choice of engine, with the attempt itself (`try_one`) and
/// the question whether a TensorRT start would build its engine
/// (`trt_would_build_on_switch`, asked only when TensorRT is to be tried)
/// passed in, so that the order and the rules are tested without a runtime.
/// Returns what `try_one` returned for the engine that started, with the
/// reasons of those that did not.
fn first_that_starts<T>(
    wanted: Engine,
    cuda: bool,
    model_id: &str,
    mut trt_would_build_on_switch: impl FnMut() -> bool,
    mut try_one: impl FnMut(Engine) -> Result<T, String>,
) -> Result<(T, Reasons), String> {
    let mut reasons = Reasons::new();
    for engine in wanted.candidates() {
        if engine.uses_cuda() && !cuda {
            // Before anything that could reach the CUDA driver (the
            // TensorRT cache key reads the device).
            log::info!(
                "ONNX cut-out: {} unavailable: {REASON_CUDA_OFF}",
                engine.label()
            );
            reasons.push((engine, REASON_CUDA_OFF.to_string()));
            continue;
        }
        if engine == Engine::TensorRt && trt_would_build_on_switch() {
            if wanted == Engine::Auto {
                // Logged with the engine used instead, once one starts.
                reasons.push((engine, REASON_TRT_NOT_BUILT.to_string()));
                continue;
            }
            log::warn!(
                "ONNX cut-out: building the TensorRT engine for {model_id} while the camera runs: the feed pauses until it is ready (this takes a while)"
            );
        }
        match try_one(engine) {
            Ok(started) => {
                if reasons
                    .iter()
                    .any(|(e, r)| *e == Engine::TensorRt && r == REASON_TRT_NOT_BUILT)
                {
                    log::warn!(
                        "ONNX cut-out: {model_id}: TensorRT engine not built yet; using {} for now. It builds at the next camera start.",
                        engine.label()
                    );
                }
                return Ok((started, reasons));
            }
            Err(reason) => {
                log::info!("ONNX cut-out: {} unavailable: {reason}", engine.label());
                reasons.push((engine, reason));
            }
        }
    }
    let list = reasons
        .iter()
        .map(|(e, r)| format!("{}: {r}", e.label()))
        .collect::<Vec<_>>()
        .join("; ");
    Err(match (wanted, reasons.as_slice()) {
        (Engine::Auto, _) => format!("No engine could run the model ({list})"),
        (e, [(_, r)]) => format!("{} was chosen and cannot run: {r}", e.label()),
        (e, _) => format!("{} was chosen and cannot run: {list}", e.label()),
    })
}

fn log_started(req: &Request<'_>, s: &Started) {
    let partly = match s.partly_on_cpu {
        Some((cpu, all)) => format!(", partly on the CPU ({cpu} of {all} kernels)"),
        None => String::new(),
    };
    log::info!(
        "ONNX cut-out: {} runs on {}{partly} (ONNX Runtime {}; session {:.0} ms, first run {:.0} ms)",
        req.model.id,
        s.engine.label(),
        req.runtime.version,
        s.load_ms,
        s.warmup_ms
    );
}

/// The reason an engine of a provider the runtime lacks cannot run.
pub const REASON_NOT_IN_BUILD: &str = "not in this ONNX Runtime build";
/// Why an engine that uses CUDA cannot run when the host turned CUDA off.
pub const REASON_CUDA_OFF: &str = "CUDA is off: the video pipeline runs in system memory";
/// Why `auto` skips TensorRT in a switch whose engine is not cached: the
/// build would hold the stream for up to a minute.
pub const REASON_TRT_NOT_BUILT: &str =
    "TensorRT engine not built yet; it builds at the next camera start";

/// Whether starting TensorRT for `req` would build its engine (none is
/// cached for this model, runtime, TensorRT and GPU), when TensorRT can
/// be tried at all.
fn tensorrt_would_build(req: &Request<'_>) -> bool {
    let name = Engine::TensorRt.provider().expect("a concrete engine");
    if !req.runtime.has_provider(name)
        || static_reasons(&req.runtime.library, req.cuda)
            .iter()
            .any(|(e, _)| *e == Engine::TensorRt)
    {
        return false;
    }
    match tensorrt_cache(req) {
        Some(d) => count_engines(&d) == 0,
        // No cache: every start builds.
        None => true,
    }
}

/// The TensorRT engine cache directory of `req`; none when the GPU cannot
/// be named (then TensorRT cannot run on it either), so that no engine is
/// ever kept under a key that does not say which GPU built it.
fn tensorrt_cache(req: &Request<'_>) -> Option<PathBuf> {
    let key = TrtKey {
        ort: &req.runtime.version,
        tensorrt: crate::runtime::tensorrt_version(&req.runtime.library),
        device: Some(crate::cuda::device_identity()?),
    };
    req.caches.tensorrt_dir(req.model, &key, req.input)
}

/// What a filesystem and environment look can say of the engines without
/// opening the runtime (for the probe's log): the provider libraries
/// beside the runtime's library, whether the host turned CUDA off (`cuda`
/// is false), and whether CUDA devices are hidden.
pub fn static_reasons(library: &Path, cuda: bool) -> Reasons {
    let dir = library.parent().unwrap_or(Path::new("/"));
    let has = |name: &str| {
        dir.join(format!("libonnxruntime_providers_{name}.so"))
            .is_file()
    };
    let cuda_hidden = std::env::var_os("CUDA_VISIBLE_DEVICES").is_some_and(|v| {
        let v = v.to_string_lossy();
        v.trim().is_empty() || v.trim() == "-1" || v.trim() == "NoDevFiles"
    });
    let no_nvidia = !Path::new("/dev/nvidiactl").exists();
    let mut out = Reasons::new();
    for engine in AUTO_ORDER {
        let lib = match engine {
            Engine::TensorRt => "tensorrt",
            Engine::Cuda => "cuda",
            Engine::OpenVinoNpu | Engine::OpenVinoGpu | Engine::OpenVinoCpu => "openvino",
            _ => continue,
        };
        let reason = if engine.uses_cuda() && !cuda {
            REASON_CUDA_OFF.to_string()
        } else if !has(lib) {
            format!(
                "{REASON_NOT_IN_BUILD} (no libonnxruntime_providers_{lib}.so beside {})",
                library.display()
            )
        } else if engine.uses_cuda() && cuda_hidden {
            "CUDA not available: CUDA_VISIBLE_DEVICES hides every GPU".to_string()
        } else if engine.uses_cuda() && no_nvidia {
            "CUDA not available: no NVIDIA device (/dev/nvidiactl)".to_string()
        } else {
            continue;
        };
        out.push((engine, reason));
    }
    out
}

/// Builds the provider's registration for `engine`.
fn provider(req: &Request<'_>, engine: Engine) -> Option<ExecutionProviderDispatch> {
    let ov = |device: &str| {
        let mut o = ep::OpenVINO::default().with_device_type(device);
        if let Some(d) = req.caches.openvino_dir() {
            o = o.with_cache_dir(d.display());
        }
        o.build().error_on_failure()
    };
    Some(match engine {
        Engine::Auto | Engine::Cpu => return None,
        Engine::TensorRt => {
            let mut t = ep::TensorRT::default().with_fp16(true);
            if let Some(d) = tensorrt_cache(req) {
                t = t
                    .with_engine_cache(true)
                    .with_engine_cache_path(d.display())
                    .with_timing_cache(true)
                    .with_timing_cache_path(d.display());
            }
            t.build().error_on_failure()
        }
        Engine::Cuda => ep::CUDA::default().build().error_on_failure(),
        Engine::OpenVinoNpu => ov("NPU"),
        Engine::OpenVinoGpu => ov("GPU"),
        Engine::OpenVinoCpu => ov("CPU"),
    })
}

fn builder_error<R>(what: &str, e: ort::Error<R>) -> String {
    format!("{what}: {e}")
}

/// A reason for `engine`, from ONNX Runtime's error and its warnings.
fn reason(engine: Engine, error: String) -> String {
    let warnings = take_warnings();
    let prefix = match engine {
        Engine::TensorRt => "TensorRT not available",
        Engine::Cuda => "CUDA not available",
        Engine::OpenVinoNpu => "OpenVINO NPU",
        Engine::OpenVinoGpu => "OpenVINO GPU",
        Engine::OpenVinoCpu => "OpenVINO CPU",
        Engine::Cpu | Engine::Auto => "CPU",
    };
    let mut r = format!("{prefix}: {}", one_line(&clean_error(&error)));
    if !warnings.is_empty() {
        r.push_str(" (ONNX Runtime: ");
        r.push_str(&one_line(&warnings.join(" / ")));
        r.push(')');
    }
    r
}

/// The useful part of an ONNX Runtime error: its C++ source locations and
/// function signatures dropped, and of a CUDA failure only the failure
/// itself (not the host name and the source line that follow it).
pub fn clean_error(s: &str) -> String {
    let s = s.trim();
    for marker in ["CUDA failure ", "CUDNN failure ", "CUBLAS failure "] {
        if let Some(i) = s.find(marker) {
            let rest = &s[i..];
            let end = rest.find(" ;").unwrap_or(rest.len());
            return rest[..end].trim().to_string();
        }
    }
    // `<file>.cc:<line> <signature> <message>`: the message is what follows
    // the last source location, past the signature's closing bracket.
    let mut rest = s;
    while let Some(i) = [".cc:", ".h:", ".cpp:"]
        .iter()
        .filter_map(|m| rest.find(m))
        .min()
    {
        let after = &rest[i..];
        let Some(sp) = after.find(' ') else { break };
        rest = after[sp..].trim_start();
    }
    if rest.len() != s.len()
        && let Some(i) = rest.rfind("] ")
    {
        rest = &rest[i + 2..];
    }
    if rest.is_empty() {
        s.to_string()
    } else {
        rest.to_string()
    }
}

fn one_line(s: &str) -> String {
    let s: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.len() > 400 {
        let mut end = 400;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    } else {
        s
    }
}

/// The current CUDA context of this thread, put back after the CUDA
/// runtime replaces it.
///
/// ONNX Runtime's CUDA and TensorRT providers call `cudaSetDevice`, which
/// makes the device's primary context current on the calling thread. For
/// a chain of CUDA frames the host has made GStreamer's context current
/// around the call, and expects it there afterwards: this guard puts it
/// back. When no context was current (a chain of host frames), it leaves
/// none current (`cuCtxSetCurrent(NULL)`), not the primary context.
pub struct CudaContextGuard {
    saved: Option<(crate::cuda::Driver, crate::cuda::Current)>,
    /// Marks the saved context as the call's (for the primary context's
    /// reset, [`crate::cuda::GpuSession`]); dropped after the restore.
    _call: Option<crate::cuda::CallContext>,
}

impl CudaContextGuard {
    /// Remembers the current context (or that there is none), if the CUDA
    /// driver is loadable; otherwise the guard does nothing.
    pub fn save(engine: Engine) -> CudaContextGuard {
        if !engine.uses_cuda() {
            return CudaContextGuard {
                saved: None,
                _call: None,
            };
        }
        let saved = crate::cuda::driver().and_then(|d| d.current().map(|c| (d, c)));
        let _call = saved.map(|(_, c)| crate::cuda::CallContext::enter(c));
        CudaContextGuard { saved, _call }
    }
}

impl Drop for CudaContextGuard {
    fn drop(&mut self) {
        if let Some((d, saved)) = self.saved.take() {
            // SAFETY: the state current when the guard was made; the host
            // keeps a saved context alive across the call the guard spans.
            unsafe { d.restore(saved) };
        }
    }
}

fn try_engine(req: &Request<'_>, engine: Engine) -> Result<Started, String> {
    let name = engine.provider().expect("a concrete engine");
    if !req.runtime.has_provider(name) {
        return Err(REASON_NOT_IN_BUILD.to_string());
    }
    take_warnings();
    // Counted before any provider is registered, dropped last (after the
    // session, on success or failure): see `GpuSession`.
    let gpu = engine.uses_cuda().then(crate::cuda::GpuSession::begin);
    let _ctx = CudaContextGuard::save(engine);
    let profile_dir = (engine != Engine::Cpu)
        .then(ProfileDir::new)
        .transpose()
        .map_err(|e| reason(engine, format!("no directory for the profile: {e}")))?;
    let t0 = Instant::now();
    let mut b: SessionBuilder = Session::builder(&req.runtime.env)
        .map_err(|e| reason(engine, format!("no session options: {e}")))?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(|e| reason(engine, builder_error("optimisation level", e)))?
        .with_intra_threads(req.threads.max(1))
        .map_err(|e| reason(engine, builder_error("threads", e)))?
        // A daemon's threads sleep between frames instead of spinning.
        .with_intra_op_spinning(false)
        .map_err(|e| reason(engine, builder_error("spinning", e)))?;
    if let Some(d) = &profile_dir {
        b = b
            .with_profiling(d.prefix(engine))
            .map_err(|e| reason(engine, builder_error("profiling", e)))?;
    }
    if let Some(p) = provider(req, engine) {
        if engine == Engine::TensorRt
            && let Some(d) = tensorrt_cache(req)
            && let Err(e) = std::fs::create_dir_all(&d)
        {
            return Err(reason(
                engine,
                format!("the engine cache {} cannot be made: {e}", d.display()),
            ));
        }
        // TensorRT leaves what it cannot build to CUDA, not to the CPU.
        let mut eps = vec![p];
        if engine == Engine::TensorRt && req.runtime.has_provider("CUDAExecutionProvider") {
            eps.push(ep::CUDA::default().build().error_on_failure());
        }
        b = b
            .with_execution_providers(eps)
            .map_err(|e| reason(engine, e.to_string()))?;
    }
    let cache_before = (engine == Engine::TensorRt)
        .then(|| tensorrt_cache(req))
        .flatten()
        .map(|d| count_engines(&d));
    let ruled_out = static_reasons(&req.runtime.library, req.cuda)
        .iter()
        .any(|(e, _)| *e == engine);
    if engine == Engine::TensorRt && cache_before == Some(0) && !ruled_out {
        log::info!(
            "ONNX cut-out: preparing the TensorRT engine for {} (first use: this takes a while)…",
            req.model.id
        );
    }
    let mut session = b
        .commit_from_memory(req.bytes)
        .map_err(|e| reason(engine, e.to_string()))?;
    let load_ms = t0.elapsed().as_secs_f32() * 1e3;
    let input_name = session
        .inputs()
        .first()
        .map(|i| i.name().to_string())
        .ok_or_else(|| "the model has no input".to_string())?;
    // The first run: the warm-up (and TensorRT's engine build, when the
    // cache has none).
    let shape = req.model.input_shape(req.input);
    let zeros = vec![0.0f32; shape.iter().product::<i64>() as usize];
    let t1 = Instant::now();
    {
        let tensor = TensorRef::from_array_view((shape, zeros.as_slice()))
            .map_err(|e| reason(engine, e.to_string()))?;
        let outputs = session
            .run(ort::inputs![input_name.as_str() => tensor])
            .map_err(|e| reason(engine, format!("the first run failed: {e}")))?;
        let (_, values) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("the model's output is not a float tensor: {e}"))?;
        let want = req.model.output_len(req.input);
        if values.len() != want {
            return Err(format!(
                "the model's output has {} values, not the {want} the catalogue expects",
                values.len()
            ));
        }
    }
    let warmup_ms = t1.elapsed().as_secs_f32() * 1e3;
    let partly_on_cpu = if profile_dir.is_some() {
        let file = session
            .end_profiling()
            .map_err(|e| reason(engine, format!("the profile could not be read: {e}")))?;
        let kernels = std::fs::read_to_string(&file)
            .ok()
            .map(|t| kernels_by_provider(&t));
        let _ = std::fs::remove_file(&file);
        match kernels {
            None => {
                log::warn!(
                    "ONNX cut-out: the profile of {} could not be read: whether all of the model runs on it is not known",
                    engine.label()
                );
                None
            }
            Some(k) => {
                let count = |f: &dyn Fn(&str) -> bool| {
                    k.iter()
                        .filter(|(p, _)| f(p))
                        .map(|(_, n)| n)
                        .sum::<usize>()
                };
                let own = count(&|p| p == name);
                let on_cpu = count(&|p| p == CPU_PROVIDER);
                let all = count(&|_| true);
                if own == 0 && engine != Engine::Cpu {
                    return Err(reason(
                        engine,
                        "the provider was registered but ran no part of the model".to_string(),
                    ));
                }
                (on_cpu > 0 && engine != Engine::Cpu).then_some((on_cpu, all))
            }
        }
    } else {
        None
    };
    let built_tensorrt_engine = matches!(cache_before, Some(0));
    Ok(Started {
        session,
        engine,
        input_name,
        partly_on_cpu,
        built_tensorrt_engine,
        load_ms,
        warmup_ms,
        _gpu: gpu,
    })
}

/// Where the profile of a session's first run is written: a directory of
/// this process's own under `$XDG_RUNTIME_DIR` (private to the user), or
/// else a new one made with mode 0700 in the temporary directory; removed
/// with what is in it when dropped. Never a predictable name in a shared
/// directory.
struct ProfileDir {
    path: PathBuf,
}

impl ProfileDir {
    fn new() -> std::io::Result<ProfileDir> {
        use std::os::unix::fs::DirBuilderExt;
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute() && p.is_dir());
        let base = runtime.unwrap_or_else(std::env::temp_dir);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let mut last = None;
        for _ in 0..16 {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = base.join(format!(
                "obc-onnx-profile-{}-{n}-{nanos:08x}",
                std::process::id()
            ));
            // `create` (not `create_all`) fails on a name that exists, so
            // the directory is always a new one of ours, mode 0700.
            match std::fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(ProfileDir { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| std::io::Error::other("no free name")))
    }

    /// The file prefix ONNX Runtime writes the profile under.
    fn prefix(&self, engine: Engine) -> PathBuf {
        self.path.join(engine.as_str())
    }
}

impl Drop for ProfileDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn count_engines(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|x| x == "engine"))
                .count()
        })
        .unwrap_or(0)
}

/// The kernels of a profile (ONNX Runtime's JSON trace), counted by the
/// provider that ran them: `(provider, kernels)`, each kernel once.
pub fn kernels_by_provider(profile: &str) -> Vec<(String, usize)> {
    let Ok(serde_json::Value::Array(events)) = serde_json::from_str::<serde_json::Value>(profile)
    else {
        return Vec::new();
    };
    let mut seen: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        Default::default();
    for e in &events {
        if e.get("cat").and_then(|c| c.as_str()) != Some("Node") {
            continue;
        }
        let Some(name) = e.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        let Some(kernel) = name.strip_suffix("_kernel_time") else {
            continue;
        };
        let Some(provider) = e
            .get("args")
            .and_then(|a| a.get("provider"))
            .and_then(|p| p.as_str())
        else {
            continue;
        };
        seen.entry(provider.to_string())
            .or_default()
            .insert(kernel.to_string());
    }
    seen.into_iter().map(|(p, k)| (p, k.len())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalogue;

    #[test]
    fn engines_round_trip_and_auto_tries_them_in_order() {
        for e in Engine::ALL {
            assert_eq!(Engine::parse(e.as_str()), Some(e));
            assert!(e.label().len() <= 48);
        }
        assert_eq!(Engine::parse("vulkan"), None);
        assert_eq!(
            Engine::Auto.candidates(),
            [
                Engine::TensorRt,
                Engine::Cuda,
                Engine::OpenVinoNpu,
                Engine::OpenVinoGpu,
                Engine::OpenVinoCpu,
                Engine::Cpu
            ]
        );
        assert_eq!(Engine::Cuda.candidates(), [Engine::Cuda]);
        assert!(Engine::TensorRt.uses_cuda() && !Engine::OpenVinoGpu.uses_cuda());
    }

    /// The engines `wanted` tries on a runtime with every provider
    /// library, with CUDA `allowed` or not, and those ruled out.
    fn tried_and_skipped(wanted: Engine, cuda: bool) -> (Vec<Engine>, Vec<(Engine, String)>) {
        let dir = tempfile::tempdir().unwrap();
        for lib in ["tensorrt", "cuda", "openvino"] {
            std::fs::write(
                dir.path()
                    .join(format!("libonnxruntime_providers_{lib}.so")),
                b"",
            )
            .unwrap();
        }
        let skipped: Vec<_> = static_reasons(&dir.path().join("libonnxruntime.so"), cuda)
            .into_iter()
            // The machine's own CUDA state is not what is tested here.
            .filter(|(_, r)| r == REASON_CUDA_OFF || !r.starts_with("CUDA not available"))
            .filter(|(e, _)| wanted.candidates().contains(e))
            .collect();
        let tried = wanted
            .candidates()
            .into_iter()
            .filter(|e| !skipped.iter().any(|(s, _)| s == e))
            .collect();
        (tried, skipped)
    }

    #[test]
    fn with_cuda_off_auto_skips_tensorrt_and_cuda_and_keeps_the_rest() {
        let (tried, skipped) = tried_and_skipped(Engine::Auto, false);
        assert_eq!(
            tried,
            [
                Engine::OpenVinoNpu,
                Engine::OpenVinoGpu,
                Engine::OpenVinoCpu,
                Engine::Cpu
            ]
        );
        assert_eq!(
            skipped,
            [
                (Engine::TensorRt, REASON_CUDA_OFF.to_string()),
                (Engine::Cuda, REASON_CUDA_OFF.to_string())
            ]
        );
        assert_eq!(
            REASON_CUDA_OFF,
            "CUDA is off: the video pipeline runs in system memory"
        );
    }

    #[test]
    fn with_cuda_off_an_explicit_cuda_engine_is_ruled_out_with_the_reason() {
        for e in [Engine::Cuda, Engine::TensorRt] {
            let (tried, skipped) = tried_and_skipped(e, false);
            assert!(tried.is_empty(), "{e:?}");
            assert_eq!(skipped, [(e, REASON_CUDA_OFF.to_string())]);
        }
    }

    #[test]
    fn with_cuda_off_the_cpu_and_openvino_engines_are_unaffected() {
        for e in [
            Engine::Cpu,
            Engine::OpenVinoNpu,
            Engine::OpenVinoGpu,
            Engine::OpenVinoCpu,
        ] {
            let (tried, skipped) = tried_and_skipped(e, false);
            assert_eq!(tried, [e]);
            assert!(skipped.is_empty(), "{e:?}: {skipped:?}");
        }
    }

    /// What [`first_that_starts`] tried and returned for `wanted` with
    /// CUDA `cuda`, every attempt failing except one on `starts` (if any).
    /// The TensorRT build question panics when CUDA is off: it reads the
    /// device, so it must not be asked then.
    fn choose(
        wanted: Engine,
        cuda: bool,
        starts: Option<Engine>,
    ) -> (Vec<Engine>, Result<(Engine, Reasons), String>) {
        let mut tried = Vec::new();
        let r = first_that_starts(
            wanted,
            cuda,
            "model",
            || {
                assert!(cuda, "the TensorRT cache was looked at with CUDA off");
                false
            },
            |e| {
                tried.push(e);
                if Some(e) == starts {
                    Ok(e)
                } else {
                    Err(format!("{e:?} failed"))
                }
            },
        );
        (tried, r)
    }

    #[test]
    fn start_with_cuda_off_never_tries_tensorrt_or_cuda() {
        // `auto`: the first engine tried is OpenVINO, and the reasons start
        // with the two CUDA engines ruled out.
        let (tried, r) = choose(Engine::Auto, false, Some(Engine::Cpu));
        assert_eq!(
            tried,
            [
                Engine::OpenVinoNpu,
                Engine::OpenVinoGpu,
                Engine::OpenVinoCpu,
                Engine::Cpu
            ]
        );
        let (engine, reasons) = r.unwrap();
        assert_eq!(engine, Engine::Cpu);
        assert_eq!(
            reasons[..2],
            [
                (Engine::TensorRt, REASON_CUDA_OFF.to_string()),
                (Engine::Cuda, REASON_CUDA_OFF.to_string())
            ]
        );
        // Chosen explicitly: nothing is tried, the error gives the reason.
        for e in [Engine::TensorRt, Engine::Cuda] {
            let (tried, r) = choose(e, false, Some(e));
            assert!(tried.is_empty(), "{e:?} tried {tried:?}");
            assert_eq!(
                r.unwrap_err(),
                format!("{} was chosen and cannot run: {REASON_CUDA_OFF}", e.label())
            );
        }
    }

    #[test]
    fn start_with_cuda_on_tries_the_cuda_engines_first() {
        let (tried, r) = choose(Engine::Auto, true, Some(Engine::Cuda));
        assert_eq!(tried, [Engine::TensorRt, Engine::Cuda]);
        let (engine, reasons) = r.unwrap();
        assert_eq!(engine, Engine::Cuda);
        assert_eq!(reasons, [(Engine::TensorRt, "TensorRt failed".to_string())]);
        // None starts: `auto`'s error lists every engine.
        let (tried, r) = choose(Engine::Auto, true, None);
        assert_eq!(tried, Engine::Auto.candidates());
        assert!(
            r.unwrap_err()
                .starts_with("No engine could run the model (")
        );
    }

    #[test]
    fn with_cuda_on_nothing_is_ruled_out_for_that_reason() {
        let (tried, skipped) = tried_and_skipped(Engine::Auto, true);
        assert_eq!(tried, Engine::Auto.candidates());
        assert!(skipped.iter().all(|(_, r)| r != REASON_CUDA_OFF));
    }

    #[test]
    fn the_tensorrt_cache_is_keyed_by_model_runtime_tensorrt_gpu_and_dynamic_size() {
        let caches = Caches::from_env(|n| match n {
            "XDG_CACHE_HOME" => Some("/c".into()),
            _ => None,
        });
        let key = TrtKey {
            ort: "1.31.1",
            tensorrt: Some("10.16.1".into()),
            device: Some(("NVIDIA GeForce RTX 5070 Ti Laptop GPU".into(), (12, 0))),
        };
        let mp = catalogue::default_model();
        assert_eq!(
            caches.tensorrt_dir(mp, &key, (144, 256)).unwrap(),
            PathBuf::from(
                "/c/omarchy-broadcast/trt/fa02222275c45563-ort1.31.1-trt10.16.1-nvidia-geforce-rtx-5070-ti-laptop-gpu-sm120"
            )
        );
        let modnet = catalogue::find("modnet").unwrap();
        let other = TrtKey {
            ort: "1.29.0",
            tensorrt: None,
            device: None,
        };
        assert_eq!(
            caches.tensorrt_dir(modnet, &other, (288, 512)).unwrap(),
            PathBuf::from(
                "/c/omarchy-broadcast/trt/07c308cf0fc7e6e8-ort1.29.0-trtunknown-unknown-gpu-288x512"
            )
        );
        // Another GPU of the same compute capability is another cache.
        let gpu2 = TrtKey {
            device: Some(("NVIDIA RTX PRO 2000".into(), (12, 0))),
            ..key.clone()
        };
        assert_ne!(
            caches.tensorrt_dir(mp, &key, (144, 256)),
            caches.tensorrt_dir(mp, &gpu2, (144, 256))
        );
        let home = Caches::from_env(|n| match n {
            "HOME" => Some("/h".into()),
            _ => None,
        });
        assert_eq!(
            home.openvino_dir().unwrap(),
            PathBuf::from("/h/.cache/omarchy-broadcast/openvino")
        );
        assert!(Caches::from_env(|_| None).root.is_none());
    }

    #[test]
    fn the_profile_goes_to_a_private_directory_of_its_own() {
        use std::os::unix::fs::PermissionsExt;
        let a = ProfileDir::new().unwrap();
        let b = ProfileDir::new().unwrap();
        assert_ne!(a.path, b.path);
        let mode = std::fs::metadata(&a.path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "{mode:o}");
        if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
            assert!(a.path.starts_with(rt), "{}", a.path.display());
        }
        let p = a.path.clone();
        std::fs::write(a.prefix(Engine::Cuda).with_extension("json"), b"[]").unwrap();
        drop(a);
        assert!(!p.exists());
    }

    #[test]
    fn a_profile_counts_kernels_per_provider() {
        let profile = r#"[
          {"cat":"Session","name":"model_loading_uri","args":{}},
          {"cat":"Node","name":"conv1_kernel_time","args":{"op_name":"Conv","provider":"CUDAExecutionProvider"}},
          {"cat":"Node","name":"conv1_kernel_time","args":{"op_name":"Conv","provider":"CUDAExecutionProvider"}},
          {"cat":"Node","name":"conv1_fence_before","args":{"op_name":"Conv"}},
          {"cat":"Node","name":"resize_kernel_time","args":{"op_name":"Resize","provider":"CPUExecutionProvider"}},
          {"cat":"Node","name":"relu_kernel_time","args":{"op_name":"Relu","provider":"CUDAExecutionProvider"}}
        ]"#;
        assert_eq!(
            kernels_by_provider(profile),
            [
                ("CPUExecutionProvider".to_string(), 1),
                ("CUDAExecutionProvider".to_string(), 2)
            ]
        );
        assert!(kernels_by_provider("not json").is_empty());
    }

    #[test]
    fn runtime_errors_are_cleaned_of_their_source_locations() {
        let cuda = "/onnxruntime_src/onnxruntime/core/providers/cuda/cuda_call.cc:154 std::conditional_t<THRW, void, onnxruntime::common::Status> onnxruntime::CudaCall(ERRTYPE, const char*, const char*, SUCCTYPE, const char*, const char*, int) [with ERRTYPE = cudaError; bool THRW = true; SUCCTYPE = cudaError; std::conditional_t<THRW, void, common::Status> = void] /onnxruntime_src/onnxruntime/core/providers/cuda/cuda_call.cc:146 std::conditional_t<THRW, void, onnxruntime::common::Status> onnxruntime::CudaCall(ERRTYPE, const char*, const char*, SUCCTYPE, const char*, const char*, int) [with ERRTYPE = cudaError; bool THRW = true; SUCCTYPE = cudaError; std::conditional_t<THRW, void, common::Status> = void] CUDA failure 100: no CUDA-capable device is detected ; GPU=-1 ; hostname=box ; file=/onnxruntime_src/x.cc ; line=280 ; expr=cudaSetDevice(info_.device_id);";
        assert_eq!(
            clean_error(cuda),
            "CUDA failure 100: no CUDA-capable device is detected"
        );
        let other = "/onnxruntime_src/onnxruntime/core/session/provider_bridge_ort.cc:1820 onnxruntime::Provider& onnxruntime::ProviderLibrary::Get() [ONNXRuntimeError] : 1 : FAIL : Failed to load library libonnxruntime_providers_cuda.so with error: libcudnn.so.9: cannot open shared object file";
        assert_eq!(
            clean_error(other),
            "[ONNXRuntimeError] : 1 : FAIL : Failed to load library libonnxruntime_providers_cuda.so with error: libcudnn.so.9: cannot open shared object file"
                .rsplit("] ")
                .next()
                .unwrap()
        );
        assert_eq!(clean_error("plain message"), "plain message");
    }

    #[test]
    fn reasons_are_one_line_and_bounded() {
        assert_eq!(one_line("a\n  b\tc"), "a b c");
        assert!(one_line(&"é".repeat(500)).chars().count() <= 401);
    }
}
