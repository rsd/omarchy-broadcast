//! The C entry points behind [`crate::export_module!`]: the vtable of a
//! [`Module`] type and the checks of the entry point. Not part of the
//! public API; the macro is.

use std::any::Any;
use std::cell::RefCell;
use std::ffi::{CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::Mutex;

use obc_module_abi::{
    OBC_ERR_PANIC, OBC_MODULE_ABI_MAJOR, OBC_MODULE_ABI_VERSION, OBC_OK, obc_chain_request,
    obc_frame, obc_host, obc_instance, obc_load_report, obc_module, obc_module_abi_major,
    obc_module_vtable, obc_param, obc_status, obc_timing,
};

use crate::log::{self, c_string};
use crate::{ChainFrames, ChainRequest, Error, ErrorKind, Frame, Instance, Module, Param};

thread_local! {
    /// The error of the last call made with a null object, or of the last
    /// failed `init`, on this thread: what `last_error(NULL)` returns. Per
    /// thread, so that the string a caller reads is never replaced by
    /// another thread's failure; it lives until the next such failure on
    /// the thread (or the thread's end).
    static ORPHAN_ERROR: RefCell<CString> = RefCell::new(CString::default());
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn set_orphan_error(e: &Error) -> obc_status {
    let text = c_string(&e.wire_message());
    ORPHAN_ERROR.with(|slot| *slot.borrow_mut() = text);
    e.status()
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "a panic without a message".to_string()
    }
}

/// Runs the module's code `f`, turning a panic into an
/// [`ErrorKind::Panic`] error.
fn guard<T>(f: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => Err(Error::new(
            ErrorKind::Panic,
            format!("the module panicked: {}", panic_message(payload.as_ref())),
        )),
    }
}

/// The outermost net of a thunk: nothing unwinds into the host, even a
/// panic of the SDK itself.
fn boundary(f: impl FnOnce() -> obc_status) -> obc_status {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(OBC_ERR_PANIC)
}

/// What an `obc_module *` points to.
struct ModuleBox<M> {
    module: M,
    /// The last JSON document returned.
    out: Mutex<CString>,
    /// The last error.
    error: Mutex<CString>,
}

impl<M> ModuleBox<M> {
    fn fail(&self, e: &Error) -> obc_status {
        *lock(&self.error) = c_string(&e.wire_message());
        e.status()
    }

    /// Keeps `json` and points `*out` to it.
    ///
    /// # Safety
    ///
    /// `out` is writable.
    unsafe fn hand_out(&self, json: String, out: *mut *const c_char) -> obc_status {
        let mut slot = lock(&self.out);
        *slot = c_string(&json);
        // SAFETY: writable by the caller's contract; the string lives
        // until the next call replaces it.
        unsafe { *out = slot.as_ptr() };
        OBC_OK
    }
}

/// What an `obc_instance *` points to.
struct InstanceBox<I> {
    instance: I,
    /// The frames of the loaded chain, when one is loaded.
    loaded: Option<ChainFrames>,
    error: CString,
    dropped: CString,
    timing_steps: Vec<CString>,
    timings: Vec<obc_timing>,
}

impl<I: Instance> InstanceBox<I> {
    fn finish(&mut self, result: Result<(), Error>) -> obc_status {
        match result {
            Ok(()) => OBC_OK,
            Err(e) => {
                self.error = c_string(&e.wire_message());
                e.status()
            }
        }
    }

    fn unload(&mut self) {
        let instance = &mut self.instance;
        if let Err(e) = guard(|| {
            instance.unload();
            Ok(())
        }) {
            log::log(log::Level::Error, e.message());
        }
        self.loaded = None;
    }
}

/// The module object behind `module`, or `None` (with the orphan error
/// set) when null.
///
/// # Safety
///
/// `module` is null or was returned by this vtable's `init` for `M` and not
/// destroyed.
unsafe fn module_ref<'a, M>(module: *const obc_module) -> Option<&'a ModuleBox<M>> {
    if module.is_null() {
        set_orphan_error(&Error::invalid("the module object is null"));
        return None;
    }
    // SAFETY: by the caller's contract.
    Some(unsafe { &*module.cast::<ModuleBox<M>>() })
}

/// The instance behind `instance`, or `None` (with the orphan error set)
/// when null.
///
/// # Safety
///
/// `instance` is null or was returned by this vtable's `instance_create`
/// for `I` and not destroyed, and no other call on it is running.
unsafe fn instance_mut<'a, I>(instance: *mut obc_instance) -> Option<&'a mut InstanceBox<I>> {
    if instance.is_null() {
        set_orphan_error(&Error::invalid("the instance is null"));
        return None;
    }
    // SAFETY: by the caller's contract.
    Some(unsafe { &mut *instance.cast::<InstanceBox<I>>() })
}

fn null_out(what: &str) -> Error {
    Error::invalid(format!("{what} is null"))
}

unsafe extern "C" fn module_init<M: Module>(
    config_json: *const c_char,
    out: *mut *mut obc_module,
) -> obc_status {
    boundary(|| {
        if out.is_null() {
            return set_orphan_error(&null_out("the output pointer of init"));
        }
        // SAFETY: non-null and writable by the ABI contract.
        unsafe { *out = ptr::null_mut() };
        let result = guard(|| {
            if config_json.is_null() {
                return Err(null_out("the configuration"));
            }
            // SAFETY: a NUL-terminated string by the ABI contract.
            let config = unsafe { std::ffi::CStr::from_ptr(config_json) }
                .to_str()
                .map_err(|_| Error::invalid("the configuration is not UTF-8"))?;
            let config: serde_json::Value = serde_json::from_str(config)
                .map_err(|e| Error::invalid(format!("the configuration is not JSON: {e}")))?;
            M::init(config)
        });
        match result {
            Ok(module) => {
                let boxed = Box::new(ModuleBox {
                    module,
                    out: Mutex::new(CString::default()),
                    error: Mutex::new(CString::default()),
                });
                // SAFETY: non-null and writable, as above.
                unsafe { *out = Box::into_raw(boxed).cast::<obc_module>() };
                OBC_OK
            }
            Err(e) => set_orphan_error(&e),
        }
    })
}

unsafe extern "C" fn module_probe<M: Module>(
    module: *mut obc_module,
    out_json: *mut *const c_char,
) -> obc_status {
    boundary(|| {
        // SAFETY: by the ABI contract.
        let Some(m) = (unsafe { module_ref::<M>(module) }) else {
            return ErrorKind::Invalid.status();
        };
        if out_json.is_null() {
            return m.fail(&null_out("the output pointer of probe"));
        }
        // SAFETY: non-null and writable by the ABI contract.
        unsafe { *out_json = ptr::null() };
        match guard(|| Ok(m.module.probe().to_json())) {
            // SAFETY: `out_json` is writable.
            Ok(json) => unsafe { m.hand_out(json, out_json) },
            Err(e) => m.fail(&e),
        }
    })
}

unsafe extern "C" fn module_check<M: Module>(
    module: *mut obc_module,
    stages: *const *const c_char,
    n_stages: usize,
) -> obc_status {
    boundary(|| {
        // SAFETY: by the ABI contract.
        let Some(m) = (unsafe { module_ref::<M>(module) }) else {
            return ErrorKind::Invalid.status();
        };
        // SAFETY: by the ABI contract, `n_stages` NUL-terminated strings,
        // valid during the call.
        let ids = match unsafe { crate::request::stage_ids(stages, n_stages) } {
            Ok(ids) => ids,
            Err(e) => return m.fail(&e),
        };
        match guard(|| m.module.check(&ids)) {
            Ok(()) => OBC_OK,
            Err(e) => m.fail(&e),
        }
    })
}

unsafe extern "C" fn module_describe_settings<M: Module>(
    module: *mut obc_module,
    out_json: *mut *const c_char,
) -> obc_status {
    boundary(|| {
        // SAFETY: by the ABI contract.
        let Some(m) = (unsafe { module_ref::<M>(module) }) else {
            return ErrorKind::Invalid.status();
        };
        if out_json.is_null() {
            return m.fail(&null_out("the output pointer of describe_settings"));
        }
        // SAFETY: non-null and writable by the ABI contract.
        unsafe { *out_json = ptr::null() };
        match guard(|| Ok(m.module.settings().to_json())) {
            // SAFETY: `out_json` is writable.
            Ok(json) => unsafe { m.hand_out(json, out_json) },
            Err(e) => m.fail(&e),
        }
    })
}

unsafe extern "C" fn module_instance_create<M: Module>(
    module: *mut obc_module,
    out: *mut *mut obc_instance,
) -> obc_status {
    boundary(|| {
        // SAFETY: by the ABI contract.
        let Some(m) = (unsafe { module_ref::<M>(module) }) else {
            return ErrorKind::Invalid.status();
        };
        if out.is_null() {
            return m.fail(&null_out("the output pointer of instance_create"));
        }
        // SAFETY: non-null and writable by the ABI contract.
        unsafe { *out = ptr::null_mut() };
        match guard(|| m.module.create_instance()) {
            Ok(instance) => {
                let boxed = Box::new(InstanceBox {
                    instance,
                    loaded: None,
                    error: CString::default(),
                    dropped: CString::default(),
                    timing_steps: Vec::new(),
                    timings: Vec::new(),
                });
                // SAFETY: as above.
                unsafe { *out = Box::into_raw(boxed).cast::<obc_instance>() };
                OBC_OK
            }
            Err(e) => m.fail(&e),
        }
    })
}

unsafe extern "C" fn instance_destroy<I: Instance>(instance: *mut obc_instance) {
    boundary(|| {
        if instance.is_null() {
            return OBC_OK;
        }
        // SAFETY: by the ABI contract, created by `instance_create` and
        // destroyed once.
        let mut boxed = unsafe { Box::from_raw(instance.cast::<InstanceBox<I>>()) };
        if boxed.loaded.is_some() {
            boxed.unload();
        }
        if let Err(e) = guard(|| {
            drop(boxed);
            Ok(())
        }) {
            log::log(log::Level::Error, e.message());
        }
        OBC_OK
    });
}

unsafe extern "C" fn instance_load<I: Instance>(
    instance: *mut obc_instance,
    request: *const obc_chain_request,
    out_report: *mut obc_load_report,
) -> obc_status {
    boundary(|| {
        // SAFETY: by the ABI contract.
        let Some(b) = (unsafe { instance_mut::<I>(instance) }) else {
            return ErrorKind::Invalid.status();
        };
        // The frames of the request, once it is read: whether a failure is
        // that of a switch.
        let mut frames = None;
        let result = (|| {
            if out_report.is_null() {
                return Err(null_out("the load report"));
            }
            // SAFETY: non-null; only the first field is read before the
            // size is known.
            let report_size = unsafe { (*out_report).struct_size } as usize;
            if report_size < size_of::<obc_load_report>() {
                return Err(Error::invalid(format!(
                    "load report struct_size {report_size} is smaller than {}",
                    size_of::<obc_load_report>()
                )));
            }
            // SAFETY: by the ABI contract, valid during the call.
            let req = unsafe { ChainRequest::from_ptr(request) }?;
            frames = Some(req.frames());
            let instance = &mut b.instance;
            match guard(|| instance.load(&req)) {
                Ok(report) => {
                    // SAFETY: writable and large enough, checked above.
                    // Field by field, so the host's `struct_size` stays.
                    unsafe {
                        (*out_report).load_ms = report.load_ms;
                        (*out_report).warmup_ms = report.warmup_ms.unwrap_or(-1.0);
                        (*out_report).reused_model = u8::from(report.reused_model);
                    }
                    Ok(())
                }
                Err(e) => Err(e),
            }
        })();
        match result {
            Ok(()) => {
                b.loaded = frames;
                OBC_OK
            }
            Err(e) => {
                // A failed switch (a load that keeps the loaded chain's
                // frames) leaves the previous chain in force, as the
                // module's `load` promises: as it was, or less the stage of
                // a stage failure. A failed first load or reload, and a
                // panic, leave the instance unloaded.
                let switch = b.loaded.is_some() && b.loaded == frames;
                if !switch || e.kind() == ErrorKind::Panic {
                    b.unload();
                }
                b.finish(Err(e))
            }
        }
    })
}

unsafe extern "C" fn instance_process<I: Instance>(
    instance: *mut obc_instance,
    input: *const obc_frame,
    output: *const obc_frame,
    live: *const obc_param,
    n_live: usize,
) -> obc_status {
    boundary(|| {
        // SAFETY: by the ABI contract.
        let Some(b) = (unsafe { instance_mut::<I>(instance) }) else {
            return ErrorKind::Invalid.status();
        };
        let result = (|| {
            // SAFETY (all three): by the ABI contract, valid during the call.
            let input = unsafe { Frame::from_ptr(input) }
                .map_err(|e| Error::new(e.kind(), format!("input: {e}")))?;
            let output = unsafe { Frame::from_ptr(output) }
                .map_err(|e| Error::new(e.kind(), format!("output: {e}")))?;
            let live = unsafe { Param::slice_from_raw(live, n_live) }?;
            if b.loaded.is_none() {
                return Err(Error::state("a frame came before a successful load"));
            }
            let instance = &mut b.instance;
            guard(|| instance.process(&input, &output, &live))
        })();
        b.finish(result)
    })
}

unsafe extern "C" fn instance_reset_temporal<I: Instance>(
    instance: *mut obc_instance,
    out_dropped_json: *mut *const c_char,
) -> obc_status {
    boundary(|| {
        // SAFETY: by the ABI contract.
        let Some(b) = (unsafe { instance_mut::<I>(instance) }) else {
            return ErrorKind::Invalid.status();
        };
        if out_dropped_json.is_null() {
            return b.finish(Err(null_out("the output pointer of reset_temporal")));
        }
        // SAFETY: non-null and writable by the ABI contract.
        unsafe { *out_dropped_json = ptr::null() };
        let instance = &mut b.instance;
        match guard(|| instance.reset_temporal()) {
            Ok(dropped) => {
                let json = serde_json::to_string(&dropped).expect("strings serialise");
                b.dropped = c_string(&json);
                // SAFETY: as above; the string lives until the next call.
                unsafe { *out_dropped_json = b.dropped.as_ptr() };
                OBC_OK
            }
            Err(e) => b.finish(Err(e)),
        }
    })
}

unsafe extern "C" fn instance_unload<I: Instance>(instance: *mut obc_instance) {
    boundary(|| {
        // SAFETY: by the ABI contract.
        if let Some(b) = unsafe { instance_mut::<I>(instance) } {
            b.unload();
        }
        OBC_OK
    });
}

unsafe extern "C" fn instance_timings<I: Instance>(
    instance: *mut obc_instance,
    out: *mut *const obc_timing,
    n: *mut usize,
) -> obc_status {
    boundary(|| {
        // SAFETY: by the ABI contract.
        let Some(b) = (unsafe { instance_mut::<I>(instance) }) else {
            return ErrorKind::Invalid.status();
        };
        if out.is_null() || n.is_null() {
            return b.finish(Err(null_out("an output pointer of timings")));
        }
        // SAFETY: non-null and writable by the ABI contract.
        unsafe {
            *out = ptr::null();
            *n = 0;
        }
        let instance = &b.instance;
        match guard(|| Ok(instance.timings())) {
            Ok(timings) => {
                b.timing_steps = timings.iter().map(|(step, _)| c_string(step)).collect();
                b.timings = b
                    .timing_steps
                    .iter()
                    .zip(&timings)
                    .map(|(step, &(_, ms))| obc_timing {
                        step: step.as_ptr(),
                        ms,
                        reserved0: 0,
                    })
                    .collect();
                if !b.timings.is_empty() {
                    // SAFETY: as above; the entries live until the next
                    // call.
                    unsafe {
                        *out = b.timings.as_ptr();
                        *n = b.timings.len();
                    }
                }
                OBC_OK
            }
            Err(e) => b.finish(Err(e)),
        }
    })
}

unsafe extern "C" fn instance_park<I: Instance>(instance: *mut obc_instance) -> obc_status {
    boundary(|| {
        // SAFETY: by the ABI contract.
        let Some(b) = (unsafe { instance_mut::<I>(instance) }) else {
            return ErrorKind::Invalid.status();
        };
        let instance = &mut b.instance;
        let result = guard(|| instance.park());
        b.finish(result)
    })
}

unsafe extern "C" fn instance_unpark<I: Instance>(instance: *mut obc_instance) -> obc_status {
    boundary(|| {
        // SAFETY: by the ABI contract.
        let Some(b) = (unsafe { instance_mut::<I>(instance) }) else {
            return ErrorKind::Invalid.status();
        };
        let instance = &mut b.instance;
        let result = guard(|| instance.unpark());
        b.finish(result)
    })
}

unsafe extern "C" fn module_last_error<M: Module>(module: *const obc_module) -> *const c_char {
    if module.is_null() {
        // The string lives in this thread's slot until the next orphan
        // error on the thread; the slot's buffer does not move when read.
        return ORPHAN_ERROR
            .try_with(|slot| slot.borrow().as_ptr())
            .unwrap_or(c"".as_ptr());
    }
    // SAFETY: by the ABI contract, a live module object.
    let m = unsafe { &*module.cast::<ModuleBox<M>>() };
    lock(&m.error).as_ptr()
}

unsafe extern "C" fn instance_last_error<I: Instance>(
    instance: *const obc_instance,
) -> *const c_char {
    if instance.is_null() {
        return c"the instance is null".as_ptr();
    }
    // SAFETY: by the ABI contract, a live instance with no other call
    // running.
    let b = unsafe { &*instance.cast::<InstanceBox<I>>() };
    b.error.as_ptr()
}

unsafe extern "C" fn module_destroy<M: Module>(module: *mut obc_module) {
    boundary(|| {
        if module.is_null() {
            return OBC_OK;
        }
        // SAFETY: by the ABI contract, created by `init` and destroyed
        // once, after all its instances.
        let boxed = unsafe { Box::from_raw(module.cast::<ModuleBox<M>>()) };
        if let Err(e) = guard(|| {
            drop(boxed);
            Ok(())
        }) {
            log::log(log::Level::Error, e.message());
        }
        OBC_OK
    });
}

/// The vtable of module type `M`.
pub const fn vtable<M: Module>() -> obc_module_vtable {
    obc_module_vtable {
        struct_size: size_of::<obc_module_vtable>() as u32,
        abi_version: OBC_MODULE_ABI_VERSION,
        init: Some(module_init::<M>),
        probe: Some(module_probe::<M>),
        describe_settings: Some(module_describe_settings::<M>),
        instance_create: Some(module_instance_create::<M>),
        instance_destroy: Some(instance_destroy::<M::Instance>),
        instance_load: Some(instance_load::<M::Instance>),
        instance_process: Some(instance_process::<M::Instance>),
        instance_reset_temporal: Some(instance_reset_temporal::<M::Instance>),
        instance_unload: Some(instance_unload::<M::Instance>),
        instance_timings: Some(instance_timings::<M::Instance>),
        instance_park: Some(instance_park::<M::Instance>),
        instance_unpark: Some(instance_unpark::<M::Instance>),
        last_error: Some(module_last_error::<M>),
        instance_last_error: Some(instance_last_error::<M::Instance>),
        destroy: Some(module_destroy::<M>),
        check: Some(module_check::<M>),
    }
}

/// The body of `obc_module_entry`: checks `host` (non-null, large enough,
/// the same ABI major version), keeps its log callback and returns
/// `vtable`; null when the host is refused.
///
/// # Safety
///
/// `host` is null or points to a readable `obc_host` of at least the
/// `struct_size` it states, valid until the library is unloaded.
pub unsafe fn entry(
    host: *const obc_host,
    vtable: &'static obc_module_vtable,
) -> *const obc_module_vtable {
    let accepted = catch_unwind(|| {
        if host.is_null() {
            return false;
        }
        // SAFETY: non-null and readable; only the first field is read
        // before the size is known.
        let size = unsafe { (*host).struct_size } as usize;
        if size < size_of::<obc_host>() {
            return false;
        }
        // SAFETY: at least as large as `obc_host`.
        let host = unsafe { &*host };
        if u32::from(obc_module_abi_major(host.abi_version)) != OBC_MODULE_ABI_MAJOR {
            return false;
        }
        log::set_host(host);
        true
    });
    if accepted.unwrap_or(false) {
        vtable
    } else {
        ptr::null()
    }
}
