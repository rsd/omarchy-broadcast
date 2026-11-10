//! Safe handles over a loaded module: the module object ([`Module`]) and its
//! instances ([`Instance`]).
//!
//! A [`Module`] is made by [`Module::open`] from a manifest and the
//! module's configuration: the library is opened and checked
//! ([`Library::open`]) and the module's `init` called with the
//! configuration as JSON. Its calls (probe, settings, instance creation)
//! are serialised by a lock; an [`Instance`] is used through `&mut self`,
//! so its calls are serialised too, and it keeps the module object (and
//! the library) alive.
//!
//! A failing call returns a [`ModuleError`]: the status, mapped to
//! [`Status`], and the module's `last_error` text. A panic of the module's
//! code is caught by the module (the SDK does it) and returned as
//! [`Status::Panic`]: the call failed, and an instance that panicked is
//! unloaded at once and refuses frames until it is loaded again.
//!
//! An [`Instance`] keeps the ABI's switch rule: a failed load on a loaded
//! instance with the same frames (a switch) keeps the previous chain, a
//! failed first load or reload leaves it unloaded. Matte stages (the
//! `background` stage and the stages of the `matte` slot) never fail open:
//! a stage failure of one is a failure of the whole chain, and the
//! instance is unloaded, so no frame runs without its matte.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString, c_char};
use std::fmt;
use std::sync::{Arc, Mutex};

use obc_backend::{Availability, HostBgr, StageId, StageList, Value};
use obc_module_abi::{
    OBC_ERR_FAILED, OBC_ERR_INVALID, OBC_ERR_PANIC, OBC_ERR_STAGE, OBC_ERR_STATE,
    OBC_ERR_UNAVAILABLE, OBC_ERR_UNSUPPORTED, OBC_FORMAT_BGR, OBC_FRAME_HOST, OBC_OK,
    obc_chain_request, obc_chroma_siting, obc_cuda, obc_format, obc_frame, obc_frame_kind,
    obc_instance, obc_load_report, obc_matrix, obc_module, obc_param, obc_plane, obc_status,
    obc_timing, obc_value,
};
use serde::Deserialize;

use crate::library::{Library, LogLevel, LogSink};
use crate::manifest::{Manifest, Slot};
use crate::settings::{SettingsDescription, parse_settings};

/// The reason of a stage the manifest declares and the probe does not
/// report.
pub const NOT_REPORTED: &str = "Not reported by the module";

/// The reason of a stage the probe reports unavailable without a reason.
pub const NO_REASON: &str = "Unavailable";

/// What kind of failure a module call is: the ABI's status codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    /// `OBC_ERR_INVALID`.
    Invalid,
    /// `OBC_ERR_UNSUPPORTED`.
    Unsupported,
    /// `OBC_ERR_UNAVAILABLE`.
    Unavailable,
    /// `OBC_ERR_FAILED`, and any code this host does not know.
    Failed,
    /// `OBC_ERR_STATE`.
    State,
    /// `OBC_ERR_PANIC`: the module's code panicked.
    Panic,
    /// `OBC_ERR_STAGE`: one stage alone failed ([`ModuleError::stage`]).
    Stage,
}

impl Status {
    /// The status of a non-zero code.
    pub fn from_raw(code: obc_status) -> Status {
        match code {
            OBC_ERR_INVALID => Status::Invalid,
            OBC_ERR_UNSUPPORTED => Status::Unsupported,
            OBC_ERR_UNAVAILABLE => Status::Unavailable,
            OBC_ERR_STATE => Status::State,
            OBC_ERR_PANIC => Status::Panic,
            OBC_ERR_STAGE => Status::Stage,
            OBC_ERR_FAILED => Status::Failed,
            _ => Status::Failed,
        }
    }
}

/// The failure of a module call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleError {
    /// The kind of failure.
    pub status: Status,
    /// The raw status code.
    pub code: obc_status,
    /// The reason: the module's `last_error` (for a stage failure, without
    /// the `<stage>: ` prefix), or the host's own.
    pub message: String,
    /// The stage that failed alone, for [`Status::Stage`].
    pub stage: Option<StageId>,
}

impl ModuleError {
    /// An error of the host's own (a call that could not be made).
    pub fn host(status: Status, message: impl Into<String>) -> ModuleError {
        let code = match status {
            Status::Invalid => OBC_ERR_INVALID,
            Status::Unsupported => OBC_ERR_UNSUPPORTED,
            Status::Unavailable => OBC_ERR_UNAVAILABLE,
            Status::Failed => OBC_ERR_FAILED,
            Status::State => OBC_ERR_STATE,
            Status::Panic => OBC_ERR_PANIC,
            Status::Stage => OBC_ERR_STAGE,
        };
        ModuleError {
            status,
            code,
            message: message.into(),
            stage: None,
        }
    }

    /// The error of status `code` with the module's text `text`. A stage
    /// failure whose text does not start with a stage id of `chain`
    /// (`<stage>: <reason>`) is [`Status::Failed`] with the whole text.
    pub fn from_call(code: obc_status, text: String, chain: &[StageId]) -> ModuleError {
        let status = Status::from_raw(code);
        if status == Status::Stage {
            if let Some((id, reason)) = text.split_once(": ")
                && let Some(stage) = chain.iter().find(|s| s.as_str() == id)
            {
                return ModuleError {
                    status,
                    code,
                    message: reason.to_string(),
                    stage: Some(stage.clone()),
                };
            }
            return ModuleError {
                status: Status::Failed,
                code,
                message: text,
                stage: None,
            };
        }
        let message = if text.is_empty() {
            format!("the module failed with status {code} and no reason")
        } else {
            text
        };
        ModuleError {
            status,
            code,
            message,
            stage: None,
        }
    }
}

impl fmt::Display for ModuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.stage {
            Some(stage) => write!(f, "{stage}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for ModuleError {}

/// Copies a module-owned string.
///
/// # Safety
///
/// `ptr` is null or a NUL-terminated string valid during the call.
unsafe fn owned_text(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: by the caller's contract.
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

/// The module object of a loaded library.
struct ModuleCore {
    /// Every instance holds an `Arc` of this, so the object is destroyed
    /// after all its instances, and the library closed after that.
    lib: Arc<Library>,
    obj: *mut obc_module,
    /// Serialises the module-level calls.
    lock: Mutex<()>,
    id: String,
    log: LogSink,
}

// SAFETY: the module object is only used under `lock` (module calls) or
// through instances, which the ABI allows to run concurrently with module
// calls; the pointer itself is never written after `init`.
unsafe impl Send for ModuleCore {}
// SAFETY: as above.
unsafe impl Sync for ModuleCore {}

impl Drop for ModuleCore {
    fn drop(&mut self) {
        let destroy = self.lib.vtable().destroy.expect("checked at open");
        // SAFETY: created by `init`, destroyed once, after every instance
        // (each holds an `Arc` of this).
        unsafe { destroy(self.obj) };
    }
}

impl ModuleCore {
    /// The module's `last_error`, copied.
    fn last_error(&self) -> String {
        let f = self.lib.vtable().last_error.expect("checked at open");
        // SAFETY: a live module object; the string is copied before the
        // next call (the lock is held by the caller).
        unsafe { owned_text(f(self.obj)) }
    }

    fn log(&self, level: LogLevel, msg: &str) {
        (self.log)(&self.id, level, msg);
    }

    /// Runs a call that hands back a JSON document.
    fn json_call(
        &self,
        what: &str,
        f: unsafe extern "C" fn(*mut obc_module, *mut *const c_char) -> obc_status,
    ) -> Result<String, ModuleError> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut out: *const c_char = std::ptr::null();
        // SAFETY: a live module object, under the lock; `out` is writable.
        let status = unsafe { f(self.obj, &mut out) };
        if status != OBC_OK {
            let mut e = ModuleError::from_call(status, self.last_error(), &[]);
            e.message = format!("{what} failed: {}", e.message);
            return Err(e);
        }
        if out.is_null() {
            return Err(ModuleError::host(
                Status::Failed,
                format!("{what} returned no document"),
            ));
        }
        // SAFETY: a NUL-terminated string owned by the module, valid until
        // the next call on it (the lock is still held).
        Ok(unsafe { owned_text(out) })
    }
}

/// A loaded module: the module object of a library, made from a manifest
/// and the module's configuration. Cheap to clone; every clone is the same
/// object.
#[derive(Clone)]
pub struct Module {
    core: Arc<ModuleCore>,
    manifest: Arc<Manifest>,
}

impl fmt::Debug for Module {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Module")
            .field("id", &self.manifest.id)
            .field("library", &self.core.lib)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct RawProbe {
    stages: Vec<RawStageProbe>,
}

#[derive(Deserialize)]
struct RawStageProbe {
    id: String,
    available: bool,
    #[serde(default)]
    reason: Option<String>,
}

/// Reads a probe report for the stages of `manifest`: one availability per
/// manifest stage, in manifest order ([`NOT_REPORTED`] for a stage missing
/// from the report). Stages the manifest does not declare, and malformed
/// entries, are reported to `ignored` and left out.
pub fn parse_probe(
    json: &str,
    manifest: &Manifest,
    mut ignored: impl FnMut(String),
) -> Result<Vec<(StageId, Availability)>, String> {
    let report: RawProbe =
        serde_json::from_str(json).map_err(|e| format!("the probe report is malformed: {e}"))?;
    let mut found: BTreeMap<String, Availability> = BTreeMap::new();
    for s in report.stages {
        if manifest.stage(&s.id).is_none() {
            ignored(format!(
                "the probe reports the stage {:?}, which the manifest does not declare; ignored",
                s.id
            ));
            continue;
        }
        let a = if s.available {
            Availability::Installed
        } else {
            Availability::NotInstalled(
                s.reason
                    .filter(|r| !r.trim().is_empty())
                    .unwrap_or_else(|| NO_REASON.to_string()),
            )
        };
        found.entry(s.id).or_insert(a);
    }
    Ok(manifest
        .stages
        .iter()
        .map(|d| {
            let a = found
                .remove(d.id.as_str())
                .unwrap_or_else(|| Availability::NotInstalled(NOT_REPORTED.to_string()));
            (d.id.clone(), a)
        })
        .collect())
}

impl Module {
    /// Opens the library of `manifest` and creates the module object from
    /// `config` (the module's configuration table; `{}` when it has none).
    /// The module's log lines and the host's about it go to `log`. The
    /// error is the reason shown for the module.
    pub fn open(
        manifest: &Manifest,
        config: &serde_json::Value,
        log: LogSink,
    ) -> Result<Module, String> {
        if let Some(reason) = manifest.unloadable_reason() {
            return Err(reason);
        }
        let lib = Library::open(&manifest.library, &manifest.id, log.clone())?;
        let config = CString::new(config.to_string())
            .map_err(|_| "the module's configuration contains a NUL byte".to_string())?;
        let init = lib.vtable().init.expect("checked at open");
        let mut obj: *mut obc_module = std::ptr::null_mut();
        // SAFETY: a checked vtable; the configuration is a NUL-terminated
        // string alive across the call, `obj` writable.
        let status = unsafe { init(config.as_ptr(), &mut obj) };
        if status != OBC_OK || obj.is_null() {
            let last_error = lib.vtable().last_error.expect("checked at open");
            // SAFETY: `last_error(NULL)` is the reason of the failed init.
            let text = unsafe { owned_text(last_error(std::ptr::null())) };
            let e = ModuleError::from_call(
                if status == OBC_OK {
                    OBC_ERR_FAILED
                } else {
                    status
                },
                text,
                &[],
            );
            return Err(format!("The module could not start: {}", e.message));
        }
        Ok(Module {
            core: Arc::new(ModuleCore {
                lib,
                obj,
                lock: Mutex::new(()),
                id: manifest.id.clone(),
                log,
            }),
            manifest: Arc::new(manifest.clone()),
        })
    }

    /// The module id.
    pub fn id(&self) -> &str {
        &self.manifest.id
    }

    /// The manifest.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The library.
    pub fn library(&self) -> &Library {
        &self.core.lib
    }

    /// Writes a log line about this module to its log.
    pub fn log(&self, level: LogLevel, msg: &str) {
        self.core.log(level, msg);
    }

    /// Asks the module, more deeply than [`Module::probe`], whether
    /// `stages` can run here (the vtable's `check`): it may open the
    /// module's own libraries, never touches the GPU. `Ok` when the module
    /// has no such check.
    pub fn check(&self, stages: &[StageId]) -> Result<(), ModuleError> {
        let Some(f) = self.core.lib.vtable().check else {
            return Ok(());
        };
        let names: Vec<CString> = stages
            .iter()
            .map(|s| CString::new(s.as_str()).expect("stage ids have no NUL byte"))
            .collect();
        let ptrs: Vec<*const c_char> = names.iter().map(|n| n.as_ptr()).collect();
        let _guard = self.core.lock.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: a live module object, under the lock; the ids live across
        // the call.
        let status = unsafe { f(self.core.obj, ptrs.as_ptr(), ptrs.len()) };
        if status != OBC_OK {
            return Err(ModuleError::from_call(status, self.core.last_error(), &[]));
        }
        Ok(())
    }

    /// The stages of the manifest that make the matte: `background` and
    /// those of the `matte` slot. They never fail open.
    pub fn matte_stages(&self) -> Vec<StageId> {
        let mut matte: Vec<StageId> = self
            .manifest
            .stages
            .iter()
            .filter(|s| s.slot == Slot::Matte)
            .map(|s| s.id.clone())
            .collect();
        if !matte.contains(&StageId::BACKGROUND) {
            matte.push(StageId::BACKGROUND);
        }
        matte
    }

    /// Asks the module which of its stages can run here: one entry per
    /// stage of the manifest, in manifest order (see [`parse_probe`]).
    /// Filesystem checks only, by the ABI's contract.
    pub fn probe(&self) -> Result<Vec<(StageId, Availability)>, ModuleError> {
        let f = self.core.lib.vtable().probe.expect("checked at open");
        let json = self.core.json_call("The probe", f)?;
        parse_probe(&json, &self.manifest, |line| {
            self.core.log(LogLevel::Warn, &line)
        })
        .map_err(|e| ModuleError::host(Status::Failed, e))
    }

    /// The module's settings, validated (see [`crate::settings`]); each
    /// setting dropped is logged.
    pub fn settings(&self) -> Result<SettingsDescription, ModuleError> {
        let f = self
            .core
            .lib
            .vtable()
            .describe_settings
            .expect("checked at open");
        let json = self.core.json_call("The settings description", f)?;
        let stages: Vec<StageId> = self.manifest.stages.iter().map(|s| s.id.clone()).collect();
        parse_settings(&json, &stages, |line| self.core.log(LogLevel::Warn, &line))
            .map_err(|e| ModuleError::host(Status::Failed, e))
    }

    /// Creates an unloaded instance.
    pub fn create_instance(&self) -> Result<Instance, ModuleError> {
        let f = self
            .core
            .lib
            .vtable()
            .instance_create
            .expect("checked at open");
        let _guard = self.core.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut raw: *mut obc_instance = std::ptr::null_mut();
        // SAFETY: a live module object, under the lock; `raw` writable.
        let status = unsafe { f(self.core.obj, &mut raw) };
        if status != OBC_OK || raw.is_null() {
            let mut e = ModuleError::from_call(
                if status == OBC_OK {
                    OBC_ERR_FAILED
                } else {
                    status
                },
                self.core.last_error(),
                &[],
            );
            e.message = format!("the instance could not be created: {}", e.message);
            return Err(e);
        }
        Ok(Instance {
            core: self.core.clone(),
            raw,
            chain: Vec::new(),
            loaded: None,
            poisoned: None,
            matte: self.matte_stages(),
        })
    }
}

/// What [`Instance::load`] loads, owned: the ABI's chain request with its
/// strings and the background image.
#[derive(Debug, Clone)]
pub struct ChainSpec {
    /// The stages, in the order they run.
    pub stages: Vec<StageId>,
    /// The `OBC_FRAME_*` kind of the frames.
    pub kind: obc_frame_kind,
    /// The `OBC_FORMAT_*` format of the input frames.
    pub input_format: obc_format,
    /// The input size.
    pub input_size: (u32, u32),
    /// The `OBC_FORMAT_*` format of the output frames.
    pub output_format: obc_format,
    /// The output size.
    pub output_size: (u32, u32),
    /// The `OBC_MATRIX_*` matrix.
    pub matrix: obc_matrix,
    /// The `OBC_CHROMA_SITING_*` siting.
    pub chroma_siting: obc_chroma_siting,
    /// Full range.
    pub full_range: bool,
    /// The CUDA context and stream of a CUDA chain (null for host frames).
    pub cuda: obc_cuda,
    /// The load-time parameters: `(stage, key, value)`.
    pub params: Vec<(StageId, String, Value)>,
    /// The background image, passed as a host BGR frame.
    pub background: Option<Arc<HostBgr>>,
}

impl ChainSpec {
    /// The frames of the chain: everything but its stages, parameters and
    /// background. A load that keeps the frames of the loaded chain is a
    /// switch.
    fn frames(&self) -> ChainFrames {
        ChainFrames {
            kind: self.kind,
            input_format: self.input_format,
            input_size: self.input_size,
            output_format: self.output_format,
            output_size: self.output_size,
            matrix: self.matrix,
            chroma_siting: self.chroma_siting,
            full_range: self.full_range,
            cuda: self.cuda,
        }
    }
}

/// See [`ChainSpec::frames`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChainFrames {
    kind: obc_frame_kind,
    input_format: obc_format,
    input_size: (u32, u32),
    output_format: obc_format,
    output_size: (u32, u32),
    matrix: obc_matrix,
    chroma_siting: obc_chroma_siting,
    full_range: bool,
    cuda: obc_cuda,
}

/// Parameters as the ABI takes them, with the strings they point to.
pub(crate) struct RawParams {
    _strings: Vec<CString>,
    params: Vec<obc_param>,
}

impl RawParams {
    /// The parameters `(stage, key, value)`; a key or string with a NUL
    /// byte is refused.
    pub(crate) fn new<'a>(
        params: impl IntoIterator<Item = (&'a str, &'a str, &'a Value)>,
    ) -> Result<RawParams, ModuleError> {
        let mut strings = Vec::new();
        let mut out = Vec::new();
        let mut keep = |s: &str| -> Result<*const c_char, ModuleError> {
            let c = CString::new(s).map_err(|_| {
                ModuleError::host(
                    Status::Invalid,
                    format!("the parameter text {s:?} has a NUL byte"),
                )
            })?;
            let ptr = c.as_ptr();
            // The heap buffer does not move with the `CString`.
            strings.push(c);
            Ok(ptr)
        };
        for (stage, key, value) in params {
            let stage = keep(stage)?;
            let key = keep(key)?;
            let value = match value {
                Value::Bool(b) => obc_value::from_bool(*b),
                Value::Int(i) => obc_value::from_int(*i),
                Value::Float(f) => obc_value::from_float(*f),
                Value::Str(s) => obc_value::from_str_ptr(keep(s)?),
            };
            out.push(obc_param { stage, key, value });
        }
        Ok(RawParams {
            _strings: strings,
            params: out,
        })
    }

    pub(crate) fn as_ptr(&self) -> *const obc_param {
        if self.params.is_empty() {
            std::ptr::null()
        } else {
            self.params.as_ptr()
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.params.len()
    }
}

/// A host BGR frame over `image`.
fn bgr_frame(image: &HostBgr) -> obc_frame {
    let mut f = obc_frame::empty();
    f.kind = OBC_FRAME_HOST;
    f.format = OBC_FORMAT_BGR;
    f.width = image.width;
    f.height = image.height;
    f.n_planes = 1;
    f.planes[0] = obc_plane {
        ptr_or_fd: image.data.as_ptr() as u64,
        pitch: image.stride as u64,
        offset: 0,
    };
    f
}

/// What a successful [`Instance::load`] reports.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoadOutcome {
    /// The load time, in milliseconds.
    pub load_ms: f32,
    /// The warm-up time, if there was one.
    pub warmup_ms: Option<f32>,
    /// Whether models of the previous load were reused.
    pub reused_model: bool,
}

/// An instance of a module: one chain of its stages for one stream.
pub struct Instance {
    core: Arc<ModuleCore>,
    raw: *mut obc_instance,
    /// The stages of the last load, to read stage failures against.
    chain: Vec<StageId>,
    /// The frames of the loaded chain, when one is loaded.
    loaded: Option<ChainFrames>,
    /// Why the instance refuses frames: it panicked.
    poisoned: Option<String>,
    /// The module's matte stages ([`Module::matte_stages`]).
    matte: Vec<StageId>,
}

// SAFETY: an instance is used through `&mut self` (or `&self` for reads
// that the type's `!Sync` keeps on one thread), so its calls are
// serialised; the ABI allows them on any thread.
unsafe impl Send for Instance {}

impl fmt::Debug for Instance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Instance")
            .field("module", &self.core.id)
            .field("chain", &self.chain)
            .field("loaded", &self.loaded)
            .finish_non_exhaustive()
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        let f = self
            .core
            .lib
            .vtable()
            .instance_destroy
            .expect("checked at open");
        // SAFETY: created by `instance_create`, destroyed once.
        unsafe { f(self.raw) };
    }
}

impl Instance {
    /// The module id.
    pub fn module_id(&self) -> &str {
        &self.core.id
    }

    /// Whether a chain is loaded, as far as the host knows.
    pub fn is_loaded(&self) -> bool {
        self.loaded.is_some()
    }

    /// The stages of the last load that succeeded or that keeps the
    /// previous chain.
    pub fn chain(&self) -> &[StageId] {
        &self.chain
    }

    fn last_error(&self) -> String {
        let f = self
            .core
            .lib
            .vtable()
            .instance_last_error
            .expect("checked at open");
        // SAFETY: a live instance; copied before the next call.
        unsafe { owned_text(f(self.raw)) }
    }

    /// The error of a failed call; a panic unloads the instance, which then
    /// refuses frames until the next successful load.
    fn fail(&mut self, status: obc_status, what: &str, chain: &[StageId]) -> ModuleError {
        let e = ModuleError::from_call(status, self.last_error(), chain);
        if e.status == Status::Panic {
            self.core.log(
                LogLevel::Error,
                &format!(
                    "the module panicked in {what}; the instance is unloaded: {}",
                    e.message
                ),
            );
            self.unload();
            self.poisoned = Some(e.message.clone());
        }
        e
    }

    /// Whether `e` must unload the instance whatever else: a stage
    /// failure of a matte stage, which never fails open, or one whose
    /// stage cannot be read (the module dropped a stage the host cannot
    /// name). Such an error becomes a failure of the whole chain
    /// ([`Status::Failed`], the stage named in the message).
    fn closes(&self, e: &mut ModuleError) -> bool {
        if e.code != OBC_ERR_STAGE {
            return false;
        }
        match e.stage.take() {
            Some(stage) if !self.matte.contains(&stage) => {
                e.stage = Some(stage);
                false
            }
            Some(stage) => {
                e.status = Status::Failed;
                e.message = format!("{stage}: {}", e.message);
                true
            }
            None => true,
        }
    }

    /// Loads `spec`. A load on a loaded instance with the frames of the
    /// loaded chain is a switch; one with other frames a reload. A failed
    /// switch keeps the previous chain (less the stage of a stage failure);
    /// a failed first load or reload, a panic, and a failure of a matte
    /// stage leave the instance unloaded (see the module documentation).
    pub fn load(&mut self, spec: &ChainSpec) -> Result<LoadOutcome, ModuleError> {
        let names: Vec<CString> = spec
            .stages
            .iter()
            .map(|s| CString::new(s.as_str()).expect("stage ids have no NUL byte"))
            .collect();
        let ptrs: Vec<*const c_char> = names.iter().map(|n| n.as_ptr()).collect();
        let params = RawParams::new(
            spec.params
                .iter()
                .map(|(s, k, v)| (s.as_str(), k.as_str(), v)),
        )?;
        let background = spec.background.as_deref().map(bgr_frame);
        let mut req = obc_chain_request::empty();
        req.stages = ptrs.as_ptr();
        req.n_stages = ptrs.len();
        req.input_kind = spec.kind;
        req.input_format = spec.input_format;
        (req.input_width, req.input_height) = spec.input_size;
        req.output_format = spec.output_format;
        (req.output_width, req.output_height) = spec.output_size;
        req.matrix = spec.matrix;
        req.chroma_siting = spec.chroma_siting;
        req.full_range = u8::from(spec.full_range);
        req.cuda = spec.cuda;
        req.params = params.as_ptr();
        req.n_params = params.len();
        req.background = background
            .as_ref()
            .map_or(std::ptr::null(), |f| f as *const obc_frame);
        let mut report = obc_load_report::empty();
        let f = self
            .core
            .lib
            .vtable()
            .instance_load
            .expect("checked at open");
        // SAFETY: a live instance; the request, its strings, parameters and
        // background frame (over `spec.background`'s data) live across the
        // call; `report` is writable.
        let status = unsafe { f(self.raw, &req, &mut report) };
        let frames = spec.frames();
        if status != OBC_OK {
            let switch = self.loaded == Some(frames);
            let mut e = self.fail(status, "a load", &spec.stages);
            if e.status == Status::Panic {
                // Unloaded by `fail`.
            } else if self.closes(&mut e) || !switch {
                // Nothing loaded, as the module agrees once told.
                self.unload();
            } else if let Some(stage) = &e.stage {
                // The previous chain, less the stage when it could not keep
                // it: the host can no longer count on it.
                self.chain.retain(|s| s != stage);
            }
            return Err(e);
        }
        self.loaded = Some(frames);
        self.poisoned = None;
        self.chain = spec.stages.clone();
        Ok(LoadOutcome {
            load_ms: report.load_ms,
            warmup_ms: (report.warmup_ms >= 0.0).then_some(report.warmup_ms),
            reused_model: report.reused_model != 0,
        })
    }

    /// Runs one frame, from `input` into `output`, with the live
    /// parameters `live` (`(stage, key, value)`).
    ///
    /// # Safety
    ///
    /// `input` and `output` describe memory of their kind that is valid for
    /// the module to read (and, for `output`, write) during the call, and
    /// that nothing else accesses meanwhile; for CUDA frames, their context
    /// is current on the calling thread.
    pub unsafe fn process(
        &mut self,
        input: &obc_frame,
        output: &obc_frame,
        live: &[(&str, &str, &Value)],
    ) -> Result<(), ModuleError> {
        if let Some(reason) = &self.poisoned {
            return Err(ModuleError::host(
                Status::State,
                format!("the module panicked earlier and must be loaded again: {reason}"),
            ));
        }
        if self.loaded.is_none() {
            return Err(ModuleError::host(
                Status::State,
                "no chain is loaded: a frame came before a successful load",
            ));
        }
        let params = RawParams::new(live.iter().copied())?;
        let f = self
            .core
            .lib
            .vtable()
            .instance_process
            .expect("checked at open");
        // SAFETY: a live instance; the frames by the caller's contract; the
        // parameters live across the call.
        let status = unsafe { f(self.raw, input, output, params.as_ptr(), params.len()) };
        if status != OBC_OK {
            let chain = self.chain.clone();
            let mut e = self.fail(status, "a frame", &chain);
            if self.closes(&mut e) {
                // The module dropped a matte stage (or one the host cannot
                // name): nothing may run without it.
                self.unload();
            } else if let Some(stage) = &e.stage {
                self.chain.retain(|s| s != stage);
            }
            return Err(e);
        }
        Ok(())
    }

    /// Forgets temporal state; returns the stages the module dropped from
    /// the chain.
    pub fn reset_temporal(&mut self) -> Result<StageList, ModuleError> {
        let f = self
            .core
            .lib
            .vtable()
            .instance_reset_temporal
            .expect("checked at open");
        let mut out: *const c_char = std::ptr::null();
        // SAFETY: a live instance; `out` writable.
        let status = unsafe { f(self.raw, &mut out) };
        if status != OBC_OK {
            let chain = self.chain.clone();
            return Err(self.fail(status, "a temporal reset", &chain));
        }
        // SAFETY: a module-owned string valid until the next call.
        let json = unsafe { owned_text(out) };
        let ids: Vec<String> = if json.trim().is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(&json).map_err(|e| {
                ModuleError::host(
                    Status::Failed,
                    format!("the temporal reset returned malformed JSON: {e}"),
                )
            })?
        };
        let mut dropped = Vec::new();
        for id in ids {
            match self.chain.iter().find(|s| s.as_str() == id) {
                Some(s) => dropped.push(s.clone()),
                None => self.core.log(
                    LogLevel::Warn,
                    &format!(
                        "the temporal reset dropped {id:?}, which is not in the chain; ignored"
                    ),
                ),
            }
        }
        if let Some(m) = dropped.iter().find(|s| self.matte.contains(s)) {
            // A matte stage that cannot keep running does not fail open.
            let e = ModuleError::host(
                Status::Failed,
                format!("the temporal reset dropped the matte stage {m}; the chain is unloaded"),
            );
            self.unload();
            return Err(e);
        }
        self.chain.retain(|s| !dropped.contains(s));
        StageList::new(dropped).map_err(|e| ModuleError::host(Status::Failed, e.to_string()))
    }

    /// Releases what is loaded. Idempotent.
    pub fn unload(&mut self) {
        let f = self
            .core
            .lib
            .vtable()
            .instance_unload
            .expect("checked at open");
        // SAFETY: a live instance.
        unsafe { f(self.raw) };
        self.loaded = None;
        self.chain.clear();
    }

    /// The step times of the last processed frame: `(step, ms)`; empty when
    /// the module reports none.
    pub fn timings(&self) -> Vec<(String, f32)> {
        let Some(f) = self.core.lib.vtable().instance_timings else {
            return Vec::new();
        };
        let (mut out, mut n): (*const obc_timing, usize) = (std::ptr::null(), 0);
        // SAFETY: a live instance; `Instance` is not `Sync`, so no other
        // call on it runs; the outputs are writable.
        let status = unsafe { f(self.raw, &mut out, &mut n) };
        if status != OBC_OK || out.is_null() || n == 0 {
            return Vec::new();
        }
        // SAFETY: `n` entries owned by the instance, valid until the next
        // call; copied now.
        let entries = unsafe { std::slice::from_raw_parts(out, n) };
        entries
            .iter()
            // SAFETY: NUL-terminated step names, as above.
            .map(|t| (unsafe { owned_text(t.step) }, t.ms))
            .collect()
    }

    /// The stream stops for a while (ADR 0017): the module may release what
    /// it rebuilds quickly. A module without the hook does nothing.
    pub fn park(&mut self) -> Result<(), ModuleError> {
        let Some(f) = self.core.lib.vtable().instance_park else {
            return Ok(());
        };
        // SAFETY: a live instance.
        let status = unsafe { f(self.raw) };
        if status != OBC_OK {
            let chain = self.chain.clone();
            return Err(self.fail(status, "a park", &chain));
        }
        Ok(())
    }

    /// The stream resumes after [`Instance::park`].
    pub fn unpark(&mut self) -> Result<(), ModuleError> {
        let Some(f) = self.core.lib.vtable().instance_unpark else {
            return Ok(());
        };
        // SAFETY: a live instance.
        let status = unsafe { f(self.raw) };
        if status != OBC_OK {
            let chain = self.chain.clone();
            return Err(self.fail(status, "an unpark", &chain));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn manifest() -> Manifest {
        Manifest::parse(
            r#"abi = 1
id = "org.x.m"
name = "M"
version = "1"
licence = "MIT"
kind = "video-stage"
library = "m.so"
[[stage]]
id = "alpha"
slot = "pre-matte"
rank = 1
device = "cpu"
frames = ["host"]
formats = ["nv12"]
[[stage]]
id = "beta"
slot = "post-composite"
rank = 1
device = "cpu"
frames = ["host"]
formats = ["nv12"]
[[stage]]
id = "gamma"
slot = "post-composite"
rank = 1
device = "cpu"
frames = ["host"]
formats = ["nv12"]
"#,
            Path::new("/m/m.toml"),
        )
        .unwrap()
    }

    #[test]
    fn a_probe_report_maps_onto_the_manifest_stages() {
        let json = r#"{"stages":[{"id":"beta","available":false,"reason":"No NPU"},
            {"id":"zeta","available":true},{"id":"alpha","available":true}]}"#;
        let mut ignored = Vec::new();
        let a = parse_probe(json, &manifest(), |l| ignored.push(l)).unwrap();
        assert_eq!(
            a,
            [
                (StageId::new("alpha").unwrap(), Availability::Installed),
                (
                    StageId::new("beta").unwrap(),
                    Availability::NotInstalled("No NPU".into())
                ),
                (
                    StageId::new("gamma").unwrap(),
                    Availability::NotInstalled(NOT_REPORTED.into())
                ),
            ]
        );
        assert_eq!(ignored.len(), 1);
        assert!(ignored[0].contains("zeta"));
        let a = parse_probe(
            r#"{"stages":[{"id":"alpha","available":false}]}"#,
            &manifest(),
            |_| {},
        )
        .unwrap();
        assert_eq!(a[0].1, Availability::NotInstalled(NO_REASON.into()));
        assert!(parse_probe("[]", &manifest(), |_| {}).is_err());
    }

    #[test]
    fn stage_failures_are_read_from_the_text() {
        let chain = [StageId::RELIGHT, StageId::DENOISE];
        let e = ModuleError::from_call(OBC_ERR_STAGE, "relight: no light".into(), &chain);
        assert_eq!(
            (e.status, e.stage.clone(), e.message.as_str()),
            (Status::Stage, Some(StageId::RELIGHT), "no light")
        );
        assert_eq!(e.to_string(), "relight: no light");
        // Not a stage of the chain, or no prefix: a failure of the chain.
        for text in ["hello: no", "no prefix"] {
            let e = ModuleError::from_call(OBC_ERR_STAGE, text.into(), &chain);
            assert_eq!(
                (e.status, e.stage, e.message.as_str()),
                (Status::Failed, None, text)
            );
        }
        let e = ModuleError::from_call(42, String::new(), &chain);
        assert_eq!(e.status, Status::Failed);
        assert!(e.message.contains("status 42"));
        assert_eq!(Status::from_raw(OBC_ERR_PANIC), Status::Panic);
    }

    #[test]
    fn params_keep_their_strings() {
        let v = [
            Value::Str("warm".into()),
            Value::Int(-5),
            Value::Float(0.5),
            Value::Bool(true),
        ];
        let raw = RawParams::new([
            ("relight", "light", &v[0]),
            ("relight", "direction", &v[1]),
            ("background", "blur_strength", &v[2]),
            ("x", "on", &v[3]),
        ])
        .unwrap();
        assert_eq!(raw.len(), 4);
        // SAFETY: the strings live in `raw`.
        unsafe {
            let p = &*raw.as_ptr();
            assert_eq!(CStr::from_ptr(p.stage).to_str().unwrap(), "relight");
            assert_eq!(CStr::from_ptr(p.key).to_str().unwrap(), "light");
            assert_eq!(CStr::from_ptr(p.value.data.s).to_str().unwrap(), "warm");
            assert_eq!((*raw.as_ptr().add(1)).value.data.i, -5);
        }
        let bad = Value::Str("a\0b".into());
        assert!(RawParams::new([("x", "k", &bad)]).is_err());
        assert!(
            RawParams::new(std::iter::empty())
                .unwrap()
                .as_ptr()
                .is_null()
        );
    }
}
