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
//! An engine chosen explicitly that cannot run (ruled out, CUDA off, or
//! failing to start) falls back to the engines after it in `auto`'s order,
//! never to one before it: a chosen OpenVINO or CPU engine never falls to
//! TensorRT or CUDA, so it never wakes the NVIDIA GPU. The load then
//! carries a notice naming the engine chosen, its reason and the engine
//! used ([`fallback_notice`]); the setting stays as it is. Only when no
//! engine from the chosen one on starts does the load fail, with an error
//! that leads with the chosen engine's reason and then lists the later
//! engines' (`"<chosen> was chosen and cannot run: <its reason>; no later
//! engine could run it (<engine>: <reason>; …)"`).
//!
//! **OpenVINO on the GPU and the NVIDIA GPU.** The OpenCL drivers the
//! process sees are limited to the Intel GPU ones when the module starts
//! ([`crate::opencl`], from [`crate::OnnxModule`]'s `init`), before any
//! session: OpenVINO's GPU device (`openvino-gpu`, chosen, reached by
//! `auto` or as a fallback) would otherwise open NVIDIA's OpenCL driver,
//! which wakes a suspended NVIDIA GPU and holds it until the process exits.
//! The `openvino-npu` and `openvino-cpu` engines do not load OpenVINO's GPU
//! plugin and are not concerned.

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

    /// The engine's name in reasons and logs.
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

    /// The option's label in the settings: what runs it, in a user's
    /// words, rather than the provider's name.
    pub fn option_label(self) -> &'static str {
        match self {
            Engine::Auto => "Automatic",
            Engine::TensorRt => "NVIDIA GPU, TensorRT (fastest)",
            Engine::Cuda => "NVIDIA GPU, CUDA",
            Engine::OpenVinoNpu => "Intel NPU (battery saver)",
            Engine::OpenVinoGpu => "Intel GPU (OpenVINO)",
            Engine::OpenVinoCpu => "CPU, OpenVINO",
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

    /// The engines a request for `self` tries, in order: for `auto`, all
    /// of [`AUTO_ORDER`]; for an engine chosen explicitly, that engine and
    /// then, as its fallbacks, the engines after it in [`AUTO_ORDER`].
    pub fn candidates(self) -> Vec<Engine> {
        match self {
            Engine::Auto => AUTO_ORDER.to_vec(),
            e => AUTO_ORDER.into_iter().skip_while(|c| *c != e).collect(),
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
/// starts; for an engine chosen explicitly that cannot run, the first of
/// its fallbacks that starts, [`Engine::candidates`]). Every engine tried
/// that did not start is logged, one line each, and returned with its
/// reason; the error, when none starts, names them.
pub fn start(req: &Request<'_>, wanted: Engine) -> Result<(Started, Reasons), String> {
    let (started, reasons) = first_that_starts(
        wanted,
        req.cuda,
        req.model.id,
        |engine| ruled_out(req, engine),
        || req.switch && tensorrt_would_build(req),
        |engine| try_engine(req, engine),
    )?;
    log_started(req, &started);
    if let Some(notice) = fallback_notice(wanted, started.engine, &reasons) {
        log::warn!("ONNX cut-out: {notice}");
    }
    Ok((started, reasons))
}

/// The notice of a load that runs on `used` when `wanted` was chosen
/// explicitly and could not run: `"<chosen> was chosen and cannot run:
/// <its reason>; using <used>"`, the reason being the one the load's error
/// would give ([`first_that_starts`]). None for `auto` and for the engine
/// chosen.
pub fn fallback_notice(wanted: Engine, used: Engine, reasons: &Reasons) -> Option<String> {
    if wanted == Engine::Auto || wanted == used {
        return None;
    }
    Some(format!(
        "{} was chosen and cannot run: {}; using {}",
        wanted.label(),
        chosen_reason(wanted, reasons),
        used.label()
    ))
}

/// [`start`]'s choice of engine, with the attempt itself (`try_one`), the
/// reason an engine is ruled out without being tried (`ruled_out`, asked
/// only of an engine CUDA being off does not rule out), and the question
/// whether a TensorRT start would build its engine
/// (`trt_would_build_on_switch`, asked only when TensorRT is to be tried)
/// passed in, so that the order and the rules are tested without a runtime.
/// Returns what `try_one` returned for the engine that started, with the
/// reasons of those that did not. An engine chosen explicitly is tried
/// first and its fallbacks after it ([`Engine::candidates`]), under the
/// same rules (CUDA off rules TensorRT and CUDA out).
fn first_that_starts<T>(
    wanted: Engine,
    cuda: bool,
    model_id: &str,
    mut ruled_out: impl FnMut(Engine) -> Option<String>,
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
        // Not tried at all: registering a provider whose library or
        // device is known to be missing fails with a rawer reason, and
        // TensorRT without its libraries crashes the process.
        if let Some(reason) = ruled_out(engine) {
            log::info!("ONNX cut-out: {} unavailable: {reason}", engine.label());
            reasons.push((engine, reason));
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
    let list = |of: &mut dyn Iterator<Item = &(Engine, String)>| {
        of.map(|(e, r)| format!("{}: {r}", e.label()))
            .collect::<Vec<_>>()
            .join("; ")
    };
    if wanted == Engine::Auto {
        return Err(format!(
            "No engine could run the model ({})",
            list(&mut reasons.iter())
        ));
    }
    let chosen = format!(
        "{} was chosen and cannot run: {}",
        wanted.label(),
        chosen_reason(wanted, &reasons)
    );
    let mut later = reasons.iter().filter(|(e, _)| *e != wanted).peekable();
    Err(if later.peek().is_none() {
        chosen
    } else {
        format!(
            "{chosen}; no later engine could run it ({})",
            list(&mut later)
        )
    })
}

/// Why `wanted`, chosen explicitly, did not start, as [`first_that_starts`]
/// recorded it.
fn chosen_reason(wanted: Engine, reasons: &Reasons) -> &str {
    reasons
        .iter()
        .find(|(e, _)| *e == wanted)
        .map_or("it did not start", |(_, r)| r.as_str())
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

/// The reason of [`static_reasons`] when `/dev/nvidiactl` is missing. Only
/// logged, never a refusal ([`ruled_out_by`]): the NVIDIA device nodes may
/// be created by the first CUDA call (`nvidia-modprobe`).
pub const REASON_NO_NVIDIA_DEVICE: &str = "CUDA not available: no NVIDIA device (/dev/nvidiactl)";

/// Why an engine that uses CUDA cannot run when the host turned CUDA off.
pub const REASON_CUDA_OFF: &str = "CUDA is off: the video pipeline runs in system memory";
/// Why `auto` skips TensorRT in a switch whose engine is not cached: the
/// build would hold the stream for up to a minute.
pub const REASON_TRT_NOT_BUILT: &str =
    "TensorRT engine not built yet; it builds at the next camera start";

/// Whether starting TensorRT for `req` would build its engine (none is
/// cached for this model, runtime, TensorRT and GPU). Asked only of a
/// TensorRT that [`ruled_out`] does not rule out; a build without the
/// provider builds nothing.
fn tensorrt_would_build(req: &Request<'_>) -> bool {
    let name = Engine::TensorRt.provider().expect("a concrete engine");
    if !req.runtime.has_provider(name) {
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

/// Why `engine` cannot start for `req`, known without trying it: a reason
/// of [`static_reasons`] that refuses ([`ruled_out_by`]), or for TensorRT,
/// that another TensorRT is in the process's global scope
/// ([`foreign_tensorrt`]) or that its libraries cannot be loaded
/// ([`tensorrt_libraries`]).
fn ruled_out(req: &Request<'_>, engine: Engine) -> Option<String> {
    let library = &req.runtime.library;
    ruled_out_by(
        engine,
        static_reasons(library, req.cuda),
        || {
            req.runtime
                .has_provider(engine.provider().expect("a concrete engine"))
        },
        || {
            let version = crate::runtime::tensorrt_version(library);
            foreign_tensorrt(
                || crate::runtime::tensorrt_provider_bound(library),
                version.as_deref(),
                crate::runtime::global_tensorrt,
            )
            .or_else(|| {
                tensorrt_libraries(
                    || {
                        let dir = library.parent().unwrap_or(Path::new("/"));
                        crate::runtime::tensorrt_names_linked(
                            &dir.join("libonnxruntime_providers_tensorrt.so"),
                        )
                    },
                    version.as_deref(),
                    || crate::cuda::device_identity().map(|(_, cc)| cc),
                    |version| {
                        crate::runtime::builder_resources_reachable(
                            version,
                            &crate::runtime::loader_dirs(),
                        )
                    },
                    // SAFETY: only TensorRT's builder resource libraries are
                    // opened here ([`crate::runtime::tensorrt_builder_resource_names`]):
                    // they hold data, their initialisers have no preconditions,
                    // and `libnvinfer` would load them for the process anyway.
                    |name| unsafe { crate::runtime::load_kept(name) },
                )
            })
        },
    )
}

/// The reason TensorRT cannot start because another TensorRT is in the
/// process's global scope, if one is: `global` gives its version and file
/// ([`crate::runtime::global_tensorrt`]), `ours` is the version of the
/// TensorRT the provider uses ([`crate::runtime::tensorrt_version`]; none
/// leaves the reason to [`tensorrt_libraries`]), and `provider_bound`
/// whether ONNX Runtime's TensorRT provider is loaded already with its
/// symbols bound at load ([`crate::runtime::tensorrt_provider_bound`];
/// asked first).
///
/// ONNX Runtime's TensorRT provider binds TensorRT's entry points, which
/// are unversioned, to the first definition the global scope has. When
/// that is another TensorRT (the Maxine SDK's 10.9 beside a provider built
/// for 11), the provider calls it, gets no builder and crashes the process
/// (`SIGSEGV`). A provider loaded and bound already is bound to its own
/// TensorRT and starts whatever has entered the global scope since. A global TensorRT
/// of the provider's own version is the one it uses (its soname matches).
fn foreign_tensorrt(
    provider_bound: impl FnOnce() -> bool,
    ours: Option<&str>,
    global: impl FnOnce() -> Option<(String, PathBuf)>,
) -> Option<String> {
    if provider_bound() {
        return None;
    }
    let ours = ours?;
    let (theirs, file) = global()?;
    (theirs != ours).then(|| {
        format!(
            "TensorRT not available: TensorRT {theirs} ({}), loaded in the daemon by another effect, is not the one ONNX Runtime's TensorRT provider uses ({ours}), and the provider would crash on it; TensorRT is used again after the daemon restarts with the cut-out selected",
            file.display()
        )
    })
}

/// [`ruled_out`]'s decision, with what it reads passed in so that it is
/// tested without a runtime or a GPU: `statics` are [`static_reasons`],
/// `has_provider` whether the runtime has `engine`'s provider, and
/// `tensorrt` the TensorRT library check ([`tensorrt_libraries`]).
///
/// A reason of `statics` refuses first, except
/// [`REASON_NO_NVIDIA_DEVICE`], which is only logged (by the probe): the
/// device nodes may be created by the first CUDA call. Then, for TensorRT
/// only and only when the runtime has its provider, the library check.
fn ruled_out_by(
    engine: Engine,
    statics: Reasons,
    has_provider: impl FnOnce() -> bool,
    tensorrt: impl FnOnce() -> Option<String>,
) -> Option<String> {
    if let Some((_, r)) = statics
        .into_iter()
        .find(|(e, r)| *e == engine && r != REASON_NO_NVIDIA_DEVICE)
    {
        return Some(r);
    }
    if engine != Engine::TensorRt || !has_provider() {
        return None;
    }
    tensorrt()
}

/// The reason TensorRT cannot start because its libraries cannot be
/// loaded, if they cannot: `version` is TensorRT's (none when no
/// `libnvinfer` of the names `linked` gives loads from the module; asked
/// only then, for the reason), `device` gives the GPU's compute
/// capability, `reachable` the builder resource libraries of a version in
/// the loader's search directories (asked only when the load fails), and
/// `load` opens a library by bare name and keeps it.
///
/// TensorRT's `libnvinfer` opens its builder resource library with
/// `dlopen` by bare name when ONNX Runtime creates a builder, for a cached
/// engine too. The provider library's `RUNPATH` (where TensorRT may be
/// found without `LD_LIBRARY_PATH`) does not apply to a `dlopen` made by
/// `libnvinfer`; without the library, `createInferBuilder` fails and ONNX
/// Runtime's TensorRT provider crashes the process (`SIGSEGV`). So the
/// library is opened here first, by the same name and from a library
/// without a `RUNPATH` (the module), so through the same search; once
/// loaded it is kept, and `libnvinfer`'s `dlopen` matches it.
///
/// When the load fails, the reason depends on what the loader's
/// directories hold (not on the loader's message, which is translated):
/// the GPU's library there, the loader's message is quoted; other builder
/// resource libraries of the version but not this GPU's, TensorRT has none
/// for it (TensorRT 11.4 ships some architectures only) and is not started,
/// as what it would do there is not known; none, they are off the path.
fn tensorrt_libraries(
    linked: impl FnOnce() -> Vec<&'static str>,
    version: Option<&str>,
    device: impl FnOnce() -> Option<(i32, i32)>,
    reachable: impl FnOnce(&str) -> Vec<String>,
    mut load: impl FnMut(&str) -> Result<(), String>,
) -> Option<String> {
    let not_found = |what: &str| {
        format!(
            "TensorRT not available: TensorRT's libraries are not on the daemon's library path ({what} not found; add TensorRT's lib directory to LD_LIBRARY_PATH)"
        )
    };
    let Some(version) = version else {
        return Some(not_found(&linked().join(" or ")));
    };
    let Some(cc) = device() else {
        return Some(
            "TensorRT not available: no NVIDIA GPU found through the CUDA driver".to_string(),
        );
    };
    let names = crate::runtime::tensorrt_builder_resource_names(version, cc);
    let mut errors = Vec::new();
    for name in &names {
        match load(name) {
            Ok(()) => return None,
            Err(e) => errors.push(e),
        }
    }
    let found = reachable(version);
    if let Some(name) = names.iter().find(|n| found.contains(n)) {
        Some(format!(
            "TensorRT not available: {name} cannot be loaded: {}",
            one_line(&errors.join("; "))
        ))
    } else if found.is_empty() {
        Some(not_found(&names[0]))
    } else {
        Some(format!(
            "TensorRT not available: TensorRT {version} has no builder library for this GPU (sm{}{})",
            cc.0, cc.1
        ))
    }
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
            not_in_build_reason(lib, library)
        } else if engine.uses_cuda() && cuda_hidden {
            "CUDA not available: CUDA_VISIBLE_DEVICES hides every GPU".to_string()
        } else if engine.uses_cuda() && no_nvidia {
            REASON_NO_NVIDIA_DEVICE.to_string()
        } else {
            continue;
        };
        out.push((engine, reason));
    }
    out
}

/// The reason for a provider library that is not beside `library`: what is
/// missing, and the package that has it (Arch's omarchy-onnxruntime names, with
/// a generic fallback for other systems).
fn not_in_build_reason(lib: &str, library: &Path) -> String {
    let (package, name) = match lib {
        "tensorrt" => ("onnxruntime-provider-tensorrt-cuda13", "TensorRT"),
        "cuda" => ("onnxruntime-provider-cuda13-*", "CUDA"),
        _ => ("onnxruntime-provider-openvino", "OpenVINO"),
    };
    format!(
        "{REASON_NOT_IN_BUILD} (no libonnxruntime_providers_{lib}.so beside {}): install {package}, or an ONNX Runtime build with the {name} provider",
        library.display()
    )
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
    // OpenVINO's GPU device lists the OpenCL drivers, NVIDIA's among them,
    // which would wake the NVIDIA GPU: they were limited when the module
    // started (`crate::opencl::limit_once`, from `init`).
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
    // Only an engine not ruled out is tried ([`first_that_starts`]): this
    // start does build.
    if engine == Engine::TensorRt && cache_before == Some(0) {
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
            assert!(e.option_label().len() <= 48);
        }
        // The names in logs and reasons are not the settings' labels.
        assert_eq!(Engine::TensorRt.label(), "NVIDIA TensorRT");
        assert_eq!(
            Engine::TensorRt.option_label(),
            "NVIDIA GPU, TensorRT (fastest)"
        );
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
        // Chosen explicitly: the engine, then those after it as fallbacks.
        assert_eq!(
            Engine::Cuda.candidates(),
            [
                Engine::Cuda,
                Engine::OpenVinoNpu,
                Engine::OpenVinoGpu,
                Engine::OpenVinoCpu,
                Engine::Cpu
            ]
        );
        assert_eq!(
            Engine::OpenVinoGpu.candidates(),
            [Engine::OpenVinoGpu, Engine::OpenVinoCpu, Engine::Cpu]
        );
        assert_eq!(Engine::Cpu.candidates(), [Engine::Cpu]);
        assert_eq!(Engine::TensorRt.candidates(), AUTO_ORDER);
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
        let others = [
            Engine::OpenVinoNpu,
            Engine::OpenVinoGpu,
            Engine::OpenVinoCpu,
            Engine::Cpu,
        ];
        let (tried, skipped) = tried_and_skipped(Engine::Cuda, false);
        assert_eq!(tried, others, "only the fallbacks are left");
        assert_eq!(skipped, [(Engine::Cuda, REASON_CUDA_OFF.to_string())]);
        let (tried, skipped) = tried_and_skipped(Engine::TensorRt, false);
        assert_eq!(tried, others);
        assert_eq!(
            skipped,
            [
                (Engine::TensorRt, REASON_CUDA_OFF.to_string()),
                (Engine::Cuda, REASON_CUDA_OFF.to_string())
            ]
        );
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
            assert_eq!(tried, e.candidates());
            assert_eq!(tried[0], e);
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
        choose_ruling_out(wanted, cuda, starts, &[])
    }

    /// [`choose`], with the engines of `out` ruled out with their reasons.
    fn choose_ruling_out(
        wanted: Engine,
        cuda: bool,
        starts: Option<Engine>,
        out: &[(Engine, &str)],
    ) -> (Vec<Engine>, Result<(Engine, Reasons), String>) {
        let mut tried = Vec::new();
        let r = first_that_starts(
            wanted,
            cuda,
            "model",
            |e| {
                assert!(cuda || !e.uses_cuda(), "{e:?} was looked at with CUDA off");
                out.iter()
                    .find(|(o, _)| *o == e)
                    .map(|(_, r)| r.to_string())
            },
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
        // Chosen explicitly: neither CUDA engine is tried, the fallbacks
        // are; when none starts, the error leads with the chosen one's
        // reason, then the later ones'.
        for e in [Engine::TensorRt, Engine::Cuda] {
            let (tried, r) = choose(e, false, Some(e));
            assert_eq!(
                tried,
                [
                    Engine::OpenVinoNpu,
                    Engine::OpenVinoGpu,
                    Engine::OpenVinoCpu,
                    Engine::Cpu
                ],
                "{e:?}"
            );
            let err = r.unwrap_err();
            let cuda_after = if e == Engine::TensorRt {
                format!("{}: {REASON_CUDA_OFF}; ", Engine::Cuda.label())
            } else {
                String::new()
            };
            let later = [
                Engine::OpenVinoNpu,
                Engine::OpenVinoGpu,
                Engine::OpenVinoCpu,
                Engine::Cpu,
            ]
            .map(|l| format!("{}: {l:?} failed", l.label()))
            .join("; ");
            assert_eq!(
                err,
                format!(
                    "{} was chosen and cannot run: {REASON_CUDA_OFF}; no later engine could \
                     run it ({cuda_after}{later})",
                    e.label()
                ),
            );
        }
    }

    /// CUDA off and `cuda` chosen: the load runs on the first OpenVINO
    /// engine that starts, with a notice that says why.
    #[test]
    fn with_cuda_off_an_explicit_cuda_falls_to_openvino_with_a_notice() {
        let (tried, r) = choose(Engine::Cuda, false, Some(Engine::OpenVinoGpu));
        assert_eq!(tried, [Engine::OpenVinoNpu, Engine::OpenVinoGpu]);
        let (engine, reasons) = r.unwrap();
        assert_eq!(engine, Engine::OpenVinoGpu);
        assert_eq!(
            fallback_notice(Engine::Cuda, engine, &reasons).as_deref(),
            Some(
                "NVIDIA CUDA was chosen and cannot run: CUDA is off: the video pipeline runs in system memory; using OpenVINO GPU"
            )
        );
    }

    /// An engine chosen explicitly falls back to the engines after it in
    /// `auto`'s order only: TensorRT to CUDA, a chosen NPU never to the
    /// NVIDIA engines (not even asked about), and the CPU, the last, to
    /// nothing.
    #[test]
    fn an_explicit_engine_falls_back_only_to_the_engines_after_it() {
        let (tried, r) = choose(Engine::TensorRt, true, Some(Engine::Cuda));
        assert_eq!(tried, [Engine::TensorRt, Engine::Cuda]);
        let (engine, reasons) = r.unwrap();
        assert_eq!(engine, Engine::Cuda);
        assert_eq!(
            fallback_notice(Engine::TensorRt, engine, &reasons).as_deref(),
            Some("NVIDIA TensorRT was chosen and cannot run: TensorRt failed; using NVIDIA CUDA")
        );

        let mut looked_at = Vec::new();
        let mut tried = Vec::new();
        let r = first_that_starts(
            Engine::OpenVinoNpu,
            true,
            "model",
            |e| {
                looked_at.push(e);
                None
            },
            || panic!("the TensorRT cache was looked at for a chosen NPU"),
            |e| {
                tried.push(e);
                Err::<Engine, _>(format!("{e:?} failed"))
            },
        );
        let fallbacks = [
            Engine::OpenVinoNpu,
            Engine::OpenVinoGpu,
            Engine::OpenVinoCpu,
            Engine::Cpu,
        ];
        assert_eq!(tried, fallbacks);
        assert_eq!(looked_at, fallbacks, "never TensorRT or CUDA");
        assert_eq!(
            r.unwrap_err(),
            format!(
                "OpenVINO NPU was chosen and cannot run: OpenVinoNpu failed; no later engine \
                 could run it ({}: OpenVinoGpu failed; {}: OpenVinoCpu failed; {}: Cpu failed)",
                Engine::OpenVinoGpu.label(),
                Engine::OpenVinoCpu.label(),
                Engine::Cpu.label()
            ),
            "the chosen engine's reason first"
        );

        let (tried, r) = choose(Engine::Cpu, true, None);
        assert_eq!(tried, [Engine::Cpu]);
        assert_eq!(
            r.unwrap_err(),
            "CPU was chosen and cannot run: Cpu failed",
            "the error as before"
        );

        // The engine chosen, started: no notice; nor for `auto`.
        let (_, r) = choose(Engine::OpenVinoGpu, true, Some(Engine::OpenVinoGpu));
        let (engine, reasons) = r.unwrap();
        assert_eq!(fallback_notice(Engine::OpenVinoGpu, engine, &reasons), None);
        let (_, r) = choose(Engine::Auto, true, Some(Engine::Cpu));
        let (engine, reasons) = r.unwrap();
        assert_eq!(fallback_notice(Engine::Auto, engine, &reasons), None);
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
    fn a_missing_provider_library_names_the_package_to_install() {
        let lib = Path::new("/usr/lib/libonnxruntime.so");
        for (p, pkg, name) in [
            (
                "tensorrt",
                "onnxruntime-provider-tensorrt-cuda13",
                "TensorRT",
            ),
            ("cuda", "onnxruntime-provider-cuda13-*", "CUDA"),
            ("openvino", "onnxruntime-provider-openvino", "OpenVINO"),
        ] {
            let r = not_in_build_reason(p, lib);
            assert!(r.starts_with(REASON_NOT_IN_BUILD), "{r}");
            assert!(
                r.contains(&format!(
                    "(no libonnxruntime_providers_{p}.so beside /usr/lib/libonnxruntime.so): install {pkg}, or an ONNX Runtime build with the {name} provider"
                )),
                "{r}"
            );
        }
    }

    #[test]
    fn an_engine_ruled_out_is_not_tried_and_its_reason_is_the_reason() {
        const NOT_IN_BUILD: &str = "not in this ONNX Runtime build (no libonnxruntime_providers_openvino.so beside /x/libonnxruntime.so)";
        let ov = [
            (Engine::OpenVinoNpu, NOT_IN_BUILD),
            (Engine::OpenVinoGpu, NOT_IN_BUILD),
            (Engine::OpenVinoCpu, NOT_IN_BUILD),
        ];
        // `auto` skips them with the reason and goes on.
        let (tried, r) = choose_ruling_out(Engine::Auto, false, Some(Engine::Cpu), &ov);
        assert_eq!(tried, [Engine::Cpu]);
        let (_, reasons) = r.unwrap();
        assert_eq!(reasons.len(), 5);
        assert!(reasons[2..].iter().all(|(_, r)| r == NOT_IN_BUILD));
        // Chosen explicitly: not tried; the fallback after the others ruled
        // out is, and the notice gives the chosen engine's reason.
        let (tried, r) = choose_ruling_out(Engine::OpenVinoNpu, true, Some(Engine::Cpu), &ov);
        assert_eq!(tried, [Engine::Cpu]);
        let (engine, reasons) = r.unwrap();
        assert_eq!(
            fallback_notice(Engine::OpenVinoNpu, engine, &reasons).unwrap(),
            format!("OpenVINO NPU was chosen and cannot run: {NOT_IN_BUILD}; using CPU")
        );
    }

    #[test]
    fn tensorrt_without_its_libraries_falls_to_cuda_and_builds_nothing() {
        const WHY: &str = "TensorRT not available: TensorRT's libraries are not on the daemon's library path (x not found)";
        let mut asked_build = false;
        let mut tried = Vec::new();
        let (engine, reasons) = first_that_starts(
            Engine::Auto,
            true,
            "model",
            |e| (e == Engine::TensorRt).then(|| WHY.to_string()),
            || {
                asked_build = true;
                true
            },
            |e| {
                tried.push(e);
                if e == Engine::Cuda {
                    Ok(e)
                } else {
                    Err("no".to_string())
                }
            },
        )
        .unwrap();
        assert_eq!(engine, Engine::Cuda);
        assert_eq!(tried, [Engine::Cuda]);
        assert_eq!(reasons, [(Engine::TensorRt, WHY.to_string())]);
        assert!(
            !asked_build,
            "a TensorRT ruled out was asked whether it builds"
        );
        // Explicit: TensorRT is not tried (nothing is built), CUDA is,
        // and the notice gives TensorRT's reason.
        let (tried, r) = choose_ruling_out(
            Engine::TensorRt,
            true,
            Some(Engine::Cuda),
            &[(Engine::TensorRt, WHY)],
        );
        assert_eq!(tried, [Engine::Cuda]);
        let (engine, reasons) = r.unwrap();
        assert_eq!(
            fallback_notice(Engine::TensorRt, engine, &reasons).unwrap(),
            format!("NVIDIA TensorRT was chosen and cannot run: {WHY}; using NVIDIA CUDA")
        );
    }

    const MAXINE_TRT: &str =
        "/opt/nvidia/maxine/videofx/external/tensorrt/lib/libnvinfer.so.10.9.0";

    fn maxine_trt() -> Option<(String, PathBuf)> {
        Some(("10.9.0".to_string(), PathBuf::from(MAXINE_TRT)))
    }

    #[test]
    fn another_tensorrt_in_the_global_scope_rules_tensorrt_out() {
        let r = foreign_tensorrt(|| false, Some("11.4.0"), maxine_trt).unwrap();
        assert_eq!(
            r,
            format!(
                "TensorRT not available: TensorRT 10.9.0 ({MAXINE_TRT}), loaded in the daemon by another effect, is not the one ONNX Runtime's TensorRT provider uses (11.4.0), and the provider would crash on it; TensorRT is used again after the daemon restarts with the cut-out selected"
            )
        );
        // A global TensorRT of the provider's own major is not refused:
        // `tensorrt_version` then reads that very library (the provider's
        // soname matches it, and the provider would load it as its own),
        // so `ours` is its version.
        assert_eq!(foreign_tensorrt(|| false, Some("10.9.0"), maxine_trt), None);
    }

    #[test]
    fn a_loaded_provider_starts_whatever_tensorrt_came_after_it() {
        let mut asked_global = false;
        let r = foreign_tensorrt(
            || true,
            Some("11.4.0"),
            || {
                asked_global = true;
                maxine_trt()
            },
        );
        assert_eq!(r, None);
        assert!(
            !asked_global,
            "a loaded provider looked at the global scope"
        );
    }

    #[test]
    fn no_global_tensorrt_or_the_providers_own_does_not_rule_out() {
        assert_eq!(foreign_tensorrt(|| false, Some("11.4.0"), || None), None);
        let own = || {
            Some((
                "11.4.0".to_string(),
                PathBuf::from("/opt/nvidia/tensorrt/lib/libnvinfer.so.11"),
            ))
        };
        assert_eq!(foreign_tensorrt(|| false, Some("11.4.0"), own), None);
        // Without the provider's TensorRT, the library check gives the
        // reason (its libraries are not found).
        assert_eq!(foreign_tensorrt(|| false, None, maxine_trt), None);
    }

    #[test]
    fn tensorrt_ruled_out_for_another_tensorrt_falls_to_cuda_with_the_reason() {
        let why = foreign_tensorrt(|| false, Some("11.4.0"), maxine_trt).unwrap();
        let (tried, r) = choose_ruling_out(
            Engine::TensorRt,
            true,
            Some(Engine::Cuda),
            &[(Engine::TensorRt, why.as_str())],
        );
        // TensorRT is never tried: trying it is the crash.
        assert_eq!(tried, [Engine::Cuda]);
        let (engine, reasons) = r.unwrap();
        assert_eq!(
            fallback_notice(Engine::TensorRt, engine, &reasons).unwrap(),
            format!("NVIDIA TensorRT was chosen and cannot run: {why}; using NVIDIA CUDA")
        );
    }

    /// [`tensorrt_libraries`] with TensorRT `version` on a GPU of compute
    /// capability `cc`, where the libraries `loads` load and `files` are
    /// the builder resource libraries in the loader's directories; also the
    /// names asked for.
    fn trt_libs(
        version: Option<&str>,
        cc: (i32, i32),
        loads: &[&str],
        files: &[&str],
    ) -> (Option<String>, Vec<String>) {
        let mut asked = Vec::new();
        let r = tensorrt_libraries(
            || vec!["libnvinfer.so.11"],
            version,
            || Some(cc),
            |v| {
                assert_eq!(Some(v), version);
                files.iter().map(|f| f.to_string()).collect()
            },
            |name| {
                asked.push(name.to_string());
                if loads.contains(&name) {
                    Ok(())
                } else {
                    // A translated message: never read for the decision.
                    Err(format!(
                        "{name}: kann die Shared-Object-Datei nicht öffnen: Datei oder Verzeichnis nicht gefunden"
                    ))
                }
            },
        );
        (r, asked)
    }

    const SM120: &str = "libnvinfer_builder_resource_sm120.so.11.4.0";
    /// TensorRT 11.4's builder resource libraries, without sm120's.
    const OTHERS: [&str; 3] = [
        "libnvinfer_builder_resource_ptx.so.11.4.0",
        "libnvinfer_builder_resource_sm86.so.11.4.0",
        "libnvinfer_builder_resource_sm90.so.11.4.0",
    ];

    #[test]
    fn tensorrt_starts_only_when_its_builder_resource_loads_by_name() {
        // On the path: TensorRT may start, and the library was loaded.
        let (r, asked) = trt_libs(Some("11.4.0"), (12, 0), &[SM120], &[SM120]);
        assert_eq!(r, None);
        assert_eq!(asked, [SM120]);
        // TensorRT 10's single library.
        let (r, _) = trt_libs(
            Some("10.9.0"),
            (12, 0),
            &["libnvinfer_builder_resource.so.10.9.0"],
            &[],
        );
        assert_eq!(r, None);
        // No `libnvinfer` from the module: nothing is loaded, the GPU is
        // not asked for, and TensorRT does not start.
        let mut asked = Vec::new();
        let r = tensorrt_libraries(
            || vec!["libnvinfer.so.11"],
            None,
            || panic!("the GPU was asked for without a TensorRT version"),
            |_| panic!("the path was looked at without a TensorRT version"),
            |name| {
                asked.push(name.to_string());
                Ok(())
            },
        );
        assert!(asked.is_empty());
        assert_eq!(
            r.unwrap(),
            "TensorRT not available: TensorRT's libraries are not on the daemon's library path (libnvinfer.so.11 not found; add TensorRT's lib directory to LD_LIBRARY_PATH)"
        );
        // No GPU to name the library by.
        let r = tensorrt_libraries(
            || vec!["libnvinfer.so.11"],
            Some("11.4.0"),
            || None,
            |_| Vec::new(),
            |_| Ok(()),
        );
        assert!(
            r.unwrap()
                .starts_with("TensorRT not available: no NVIDIA GPU")
        );
    }

    #[test]
    fn the_reason_tensorrt_does_not_load_comes_from_the_files_on_the_path() {
        // None on the path (TensorRT found through the provider's RUNPATH
        // only): the reason names the library and the fix.
        let (r, _) = trt_libs(Some("11.4.0"), (12, 0), &[], &[]);
        assert_eq!(
            r.unwrap(),
            "TensorRT not available: TensorRT's libraries are not on the daemon's library path (libnvinfer_builder_resource_sm120.so.11.4.0 not found; add TensorRT's lib directory to LD_LIBRARY_PATH)"
        );
        // TensorRT's libraries on the path, but none for this GPU (an
        // architecture TensorRT 11.4 does not ship): still refused, as
        // what TensorRT does there is not known, with the true reason.
        let (r, asked) = trt_libs(Some("11.4.0"), (12, 1), &[], &OTHERS);
        assert_eq!(asked, ["libnvinfer_builder_resource_sm121.so.11.4.0"]);
        assert_eq!(
            r.unwrap(),
            "TensorRT not available: TensorRT 11.4.0 has no builder library for this GPU (sm121)"
        );
        let (r, _) = trt_libs(Some("11.4.0"), (10, 3), &[], &OTHERS);
        assert!(r.unwrap().ends_with("for this GPU (sm103)"));
        // The GPU's library on the path and not loading: the loader's
        // message is quoted, whatever its language.
        let mut files = OTHERS.to_vec();
        files.push(SM120);
        let (r, _) = trt_libs(Some("11.4.0"), (12, 0), &[], &files);
        assert_eq!(
            r.unwrap(),
            "TensorRT not available: libnvinfer_builder_resource_sm120.so.11.4.0 cannot be loaded: libnvinfer_builder_resource_sm120.so.11.4.0: kann die Shared-Object-Datei nicht öffnen: Datei oder Verzeichnis nicht gefunden"
        );
        // TensorRT 10: the library on the path is the one named.
        let (r, _) = trt_libs(
            Some("10.9.0"),
            (12, 0),
            &[],
            &["libnvinfer_builder_resource_sm120.so.10.9.0"],
        );
        assert!(r.unwrap().starts_with(
            "TensorRT not available: libnvinfer_builder_resource_sm120.so.10.9.0 cannot be loaded: "
        ));
    }

    /// [`ruled_out_by`] for `engine` with `statics`, the provider in the
    /// build or not, and the TensorRT check giving `trt`; also whether the
    /// provider and the TensorRT check were asked.
    fn rule(
        engine: Engine,
        statics: &[(Engine, &str)],
        provider: bool,
        trt: Option<&str>,
    ) -> (Option<String>, bool, bool) {
        let (mut asked_provider, mut asked_trt) = (false, false);
        let r = ruled_out_by(
            engine,
            statics.iter().map(|(e, r)| (*e, r.to_string())).collect(),
            || {
                asked_provider = true;
                provider
            },
            || {
                asked_trt = true;
                trt.map(str::to_string)
            },
        );
        (r, asked_provider, asked_trt)
    }

    #[test]
    fn a_static_reason_rules_out_before_the_tensorrt_libraries_are_looked_at() {
        const HIDDEN: &str = "CUDA not available: CUDA_VISIBLE_DEVICES hides every GPU";
        const LIBS: &str = "TensorRT not available: libraries";
        let statics = [(Engine::TensorRt, HIDDEN), (Engine::Cuda, HIDDEN)];
        let (r, _, asked_trt) = rule(Engine::TensorRt, &statics, true, Some(LIBS));
        assert_eq!(r.as_deref(), Some(HIDDEN));
        assert!(!asked_trt, "the libraries were looked at past a refusal");
        let (r, _, _) = rule(Engine::Cuda, &statics, true, Some(LIBS));
        assert_eq!(r.as_deref(), Some(HIDDEN));
        // Not in the build: refused too.
        let not_built = format!("{REASON_NOT_IN_BUILD} (no x)");
        let (r, _, asked_trt) = rule(
            Engine::TensorRt,
            &[(Engine::TensorRt, &not_built)],
            false,
            Some(LIBS),
        );
        assert_eq!(r, Some(not_built));
        assert!(!asked_trt);
        // No static reason: TensorRT's library check is the reason.
        let (r, asked_provider, asked_trt) = rule(Engine::TensorRt, &[], true, Some(LIBS));
        assert_eq!(r.as_deref(), Some(LIBS));
        assert!(asked_provider && asked_trt);
        let (r, _, _) = rule(Engine::TensorRt, &[], true, None);
        assert_eq!(r, None);
    }

    #[test]
    fn the_tensorrt_libraries_are_looked_at_only_for_a_tensorrt_in_the_build() {
        let (r, asked_provider, asked_trt) = rule(Engine::TensorRt, &[], false, Some("x"));
        assert_eq!(r, None);
        assert!(asked_provider && !asked_trt);
        for e in [Engine::Cuda, Engine::OpenVinoGpu, Engine::Cpu] {
            let (r, _, asked_trt) = rule(e, &[], true, Some("x"));
            assert_eq!(r, None, "{e:?}");
            assert!(!asked_trt, "{e:?}");
        }
    }

    #[test]
    fn a_missing_nvidia_device_node_is_logged_and_does_not_refuse() {
        let statics = [
            (Engine::TensorRt, REASON_NO_NVIDIA_DEVICE),
            (Engine::Cuda, REASON_NO_NVIDIA_DEVICE),
        ];
        // CUDA is tried: the first CUDA call may create the node.
        let (r, _, _) = rule(Engine::Cuda, &statics, true, None);
        assert_eq!(r, None);
        // TensorRT goes on to its library check.
        let (r, _, asked_trt) = rule(Engine::TensorRt, &statics, true, None);
        assert_eq!(r, None);
        assert!(asked_trt);
        let (r, _, _) = rule(Engine::TensorRt, &statics, true, Some("libs"));
        assert_eq!(r.as_deref(), Some("libs"));
        // static_reasons gives that reason by the constant.
        assert_eq!(
            REASON_NO_NVIDIA_DEVICE,
            "CUDA not available: no NVIDIA device (/dev/nvidiactl)"
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
