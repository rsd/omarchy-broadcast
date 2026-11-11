//! Drives a module exported with `export_module!` exactly as a C host
//! does: through the exported symbols and the vtable's function pointers,
//! with raw ABI structures.

use std::ffi::{CStr, c_char, c_void};
use std::ptr;
use std::sync::{Mutex, OnceLock};

use obc_module_sdk::abi::*;
use obc_module_sdk::{
    Apply, ChainRequest, Error, Frame, FrameKind, Group, Instance, LoadReport, Module, Op, Param,
    PixelFormat, ProbeReport, Role, Setting, SettingsDescription, Value,
};

// ---------------------------------------------------------------------------
// The module under test
// ---------------------------------------------------------------------------

/// Adds `step` (from its configuration) to every luma byte of a host NV12
/// frame, in place, except on a repeat (`OBC_FRAME_FLAG_REPEAT`), which it
/// leaves as it came; panics on the live parameter `test.panic`.
struct TestModule {
    step: u8,
}

struct TestInstance {
    step: u8,
    stages: Vec<String>,
    frames: u32,
}

impl Module for TestModule {
    type Instance = TestInstance;

    fn init(config: obc_module_sdk::serde_json::Value) -> Result<Self, Error> {
        if config.get("fail").is_some() {
            return Err(Error::unavailable("asked to fail"));
        }
        let step = config.get("step").and_then(|v| v.as_u64()).unwrap_or(1) as u8;
        obc_module_sdk::log::info!("test module initialised with step {step}");
        Ok(TestModule { step })
    }

    fn probe(&self) -> ProbeReport {
        ProbeReport::new()
            .available("test")
            .unavailable("missing", "Not installed")
    }

    fn settings(&self) -> SettingsDescription {
        SettingsDescription::new().group(
            Group::new("test", "Test")
                .setting(
                    Setting::bool("effect.test.enabled", "Test", false)
                        .stage("test")
                        .role(Role::Enable),
                )
                .setting(
                    Setting::int("effect.test.amount", "Amount", 0, 10, 1, 5)
                        .stage("test")
                        .key("amount")
                        .apply(Apply::Live)
                        .enabled_when("effect.test.enabled", Op::Eq, true),
                ),
        )
    }

    fn check(&self, stages: &[&str]) -> Result<(), Error> {
        match stages.iter().find(|s| **s != "test") {
            Some(s) => Err(Error::unavailable(format!("the runtime of {s} is missing"))),
            None => Ok(()),
        }
    }

    fn create_instance(&self) -> Result<TestInstance, Error> {
        Ok(TestInstance {
            step: self.step,
            stages: Vec::new(),
            frames: 0,
        })
    }
}

impl Instance for TestInstance {
    fn load(&mut self, req: &ChainRequest<'_>) -> Result<LoadReport, Error> {
        if req.input_kind() != FrameKind::Host || req.input_format() != PixelFormat::Nv12 {
            return Err(Error::unsupported("host NV12 only"));
        }
        if req.param("test", "fail_stage") == Some(Value::Bool(true)) {
            // A stage alone fails: what was loaded stays.
            return Err(Error::stage("test", "the stage could not take it"));
        }
        if req.param("test", "fail") == Some(Value::Bool(true)) {
            return Err(Error::failed("the chain could not load"));
        }
        if req.param("test", "panic") == Some(Value::Bool(true)) {
            panic!("the load was told to panic");
        }
        let reused = !self.stages.is_empty();
        self.stages = req.stages().iter().map(|s| s.to_string()).collect();
        Ok(LoadReport {
            load_ms: 1.5,
            warmup_ms: None,
            reused_model: reused,
        })
    }

    fn process(
        &mut self,
        input: &Frame<'_>,
        output: &Frame<'_>,
        live: &[Param<'_>],
    ) -> Result<(), Error> {
        if Param::find(live, "test", "panic") == Some(Value::Bool(true)) {
            panic!("boom at frame {}", self.frames);
        }
        assert!(input.same_memory(output));
        if input.is_repeat() {
            return Ok(());
        }
        // SAFETY: the host passes a host frame it owns for the call, in
        // place; only the output's plane is touched.
        let y = unsafe { output.host_plane_mut(0) }?;
        for b in y.iter_mut() {
            *b = b.wrapping_add(self.step);
        }
        self.frames += 1;
        Ok(())
    }

    fn reset_temporal(&mut self) -> Result<Vec<String>, Error> {
        Ok(vec!["test".to_string()])
    }

    fn unload(&mut self) {
        self.stages.clear();
    }

    fn timings(&self) -> Vec<(String, f32)> {
        vec![("test".to_string(), 0.25)]
    }
}

obc_module_sdk::export_module!(TestModule);

// ---------------------------------------------------------------------------
// A host
// ---------------------------------------------------------------------------

/// Held by the tests that set the error of null objects and failed inits
/// (`last_error(NULL)`), which is shared by the whole library.
static ORPHAN: Mutex<()> = Mutex::new(());

static LOG: Mutex<Vec<(u32, String)>> = Mutex::new(Vec::new());

unsafe extern "C" fn host_log(user: *mut c_void, level: u32, msg: *const c_char) {
    assert_eq!(user as usize, 0x5eed);
    // SAFETY: the module passes a NUL-terminated string.
    let msg = unsafe { CStr::from_ptr(msg) }
        .to_string_lossy()
        .into_owned();
    LOG.lock().unwrap().push((level, msg));
}

fn host() -> &'static obc_host {
    // The address of a leaked host: `obc_host` holds raw pointers, so it
    // is not `Sync` and cannot sit in a static itself.
    static HOST: OnceLock<usize> = OnceLock::new();
    let addr = *HOST.get_or_init(|| {
        let mut host = obc_host::empty();
        host.user = 0x5eed as *mut c_void;
        host.log = Some(host_log);
        Box::leak(Box::new(host)) as *const obc_host as usize
    });
    // SAFETY: leaked, so valid forever, and never written.
    unsafe { &*(addr as *const obc_host) }
}

fn vtable() -> &'static obc_module_vtable {
    // SAFETY: the host lives forever.
    let vt = unsafe { obc_module_entry(host()) };
    assert!(!vt.is_null());
    // SAFETY: the vtable is static.
    unsafe { &*vt }
}

fn text(ptr: *const c_char) -> String {
    assert!(!ptr.is_null());
    // SAFETY: the module returns NUL-terminated strings.
    unsafe { CStr::from_ptr(ptr) }.to_str().unwrap().to_string()
}

fn init(vt: &obc_module_vtable, config: &CStr) -> *mut obc_module {
    let mut module = ptr::null_mut();
    // SAFETY: valid arguments.
    let status = unsafe { vt.init.unwrap()(config.as_ptr(), &mut module) };
    assert_eq!(
        status,
        OBC_OK,
        "{}",
        text(unsafe { vt.last_error.unwrap()(ptr::null()) })
    );
    assert!(!module.is_null());
    module
}

fn create(vt: &obc_module_vtable, module: *mut obc_module) -> *mut obc_instance {
    let mut instance = ptr::null_mut();
    // SAFETY: a live module.
    assert_eq!(
        unsafe { vt.instance_create.unwrap()(module, &mut instance) },
        OBC_OK
    );
    instance
}

/// A host NV12 frame of `width` x `height` over `buf`, tightly packed.
fn nv12(buf: &mut [u8], width: u32, height: u32) -> obc_frame {
    let mut f = obc_frame::empty();
    f.kind = OBC_FRAME_HOST;
    f.format = OBC_FORMAT_NV12;
    f.width = width;
    f.height = height;
    f.n_planes = 2;
    let base = buf.as_mut_ptr() as u64;
    f.planes[0] = obc_plane {
        ptr_or_fd: base,
        pitch: width as u64,
        offset: 0,
    };
    f.planes[1] = obc_plane {
        ptr_or_fd: base,
        pitch: width as u64,
        offset: (width * height) as u64,
    };
    f
}

fn load(vt: &obc_module_vtable, instance: *mut obc_instance) -> (obc_status, obc_load_report) {
    let stages = [c"test".as_ptr()];
    let mut req = obc_chain_request::empty();
    req.stages = stages.as_ptr();
    req.n_stages = 1;
    req.input_kind = OBC_FRAME_HOST;
    req.input_format = OBC_FORMAT_NV12;
    req.input_width = 4;
    req.input_height = 2;
    req.output_format = OBC_FORMAT_NV12;
    req.output_width = 4;
    req.output_height = 2;
    req.matrix = OBC_MATRIX_BT601;
    req.chroma_siting = OBC_CHROMA_SITING_MPEG2;
    let mut report = obc_load_report::empty();
    // SAFETY: everything lives across the call.
    let status = unsafe { vt.instance_load.unwrap()(instance, &req, &mut report) };
    (status, report)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn abi_version_is_packed() {
    assert_eq!(obc_module_abi_version(), obc_module_abi_pack(1, 1));
    assert_eq!(obc_module_abi_major(obc_module_abi_version()), 1);
    let vt = vtable();
    assert_eq!(vt.struct_size as usize, size_of::<obc_module_vtable>());
    assert_eq!(vt.abi_version, OBC_MODULE_ABI_VERSION);
    assert!(vt.instance_park.is_some() && vt.destroy.is_some());
}

#[test]
fn entry_refuses_a_bad_host() {
    // SAFETY: null is allowed.
    assert!(unsafe { obc_module_entry(ptr::null()) }.is_null());
    let mut small = obc_host::empty();
    small.struct_size = 4;
    // SAFETY: a live host.
    assert!(unsafe { obc_module_entry(&small) }.is_null());
    let mut other_major = obc_host::empty();
    other_major.abi_version = obc_module_abi_pack(2, 0);
    // SAFETY: as above.
    assert!(unsafe { obc_module_entry(&other_major) }.is_null());
    let mut newer_minor = *host();
    newer_minor.abi_version = obc_module_abi_pack(1, 3);
    // SAFETY: as above.
    assert!(!unsafe { obc_module_entry(&newer_minor) }.is_null());
}

#[test]
fn init_probe_and_describe_round_trip() {
    let vt = vtable();
    let module = init(vt, c"{\"step\": 3}");
    assert!(
        LOG.lock()
            .unwrap()
            .iter()
            .any(|(level, msg)| *level == OBC_LOG_INFO
                && msg == "test module initialised with step 3")
    );

    let mut json = ptr::null();
    // SAFETY: a live module.
    assert_eq!(unsafe { vt.probe.unwrap()(module, &mut json) }, OBC_OK);
    let probe = ProbeReport::from_json(&text(json)).unwrap();
    assert_eq!(
        probe,
        ProbeReport::new()
            .available("test")
            .unavailable("missing", "Not installed")
    );

    // SAFETY: as above.
    assert_eq!(
        unsafe { vt.describe_settings.unwrap()(module, &mut json) },
        OBC_OK
    );
    let value: obc_module_sdk::serde_json::Value =
        obc_module_sdk::serde_json::from_str(&text(json)).unwrap();
    assert_eq!(value["groups"][0]["id"], "test");
    assert_eq!(value["groups"][0]["settings"][1]["type"], "int");
    assert_eq!(value["groups"][0]["settings"][1]["apply"], "live");
    assert_eq!(
        value["groups"][0]["settings"][1]["enabled_when"][0],
        obc_module_sdk::serde_json::json!(["effect.test.enabled", "eq", true])
    );

    // SAFETY: as above; destroyed once.
    unsafe { vt.destroy.unwrap()(module) };
}

#[test]
fn check_runs_the_modules_deeper_check() {
    let vt = vtable();
    let module = init(vt, c"{}");
    let check = vt.check.expect("the SDK sets check");
    let ok = [c"test".as_ptr()];
    // SAFETY: a live module; the ids live across the call.
    assert_eq!(unsafe { check(module, ok.as_ptr(), ok.len()) }, OBC_OK);
    // SAFETY: no stage at all.
    assert_eq!(unsafe { check(module, ptr::null(), 0) }, OBC_OK);
    let missing = [c"test".as_ptr(), c"missing".as_ptr()];
    // SAFETY: as above.
    assert_eq!(
        unsafe { check(module, missing.as_ptr(), missing.len()) },
        OBC_ERR_UNAVAILABLE
    );
    // SAFETY: a live module.
    assert_eq!(
        text(unsafe { vt.last_error.unwrap()(module) }),
        "the runtime of missing is missing"
    );
    // SAFETY: refused before any read.
    assert_eq!(unsafe { check(module, ptr::null(), 2) }, OBC_ERR_INVALID);
    // SAFETY: destroyed once.
    unsafe { vt.destroy.unwrap()(module) };
}

#[test]
fn the_error_of_a_failed_init_belongs_to_its_thread() {
    let vt = vtable();
    let init = vt.init.unwrap();
    let last = vt.last_error.unwrap();
    let mut module = ptr::null_mut();
    // SAFETY: valid pointers.
    assert_eq!(
        unsafe { init(c"{\"fail\": true}".as_ptr(), &mut module) },
        OBC_ERR_UNAVAILABLE
    );
    // SAFETY: null asks for this thread's last failed init.
    let mine = unsafe { last(ptr::null()) };
    // Another thread's failure replaces neither the text nor its memory.
    std::thread::spawn(move || {
        let mut module = ptr::null_mut();
        // SAFETY: valid pointers.
        let status = unsafe { init(c"not json".as_ptr(), &mut module) };
        assert_eq!(status, OBC_ERR_INVALID);
    })
    .join()
    .unwrap();
    assert_eq!(text(mine), "asked to fail");
}

#[test]
fn init_failures_are_reported() {
    let _orphan = ORPHAN.lock().unwrap_or_else(|e| e.into_inner());
    let vt = vtable();
    let init = vt.init.unwrap();
    let last = vt.last_error.unwrap();
    let mut module = ptr::dangling_mut::<obc_module>();
    // SAFETY: valid pointers.
    assert_eq!(
        unsafe { init(c"{\"fail\": true}".as_ptr(), &mut module) },
        OBC_ERR_UNAVAILABLE
    );
    assert!(module.is_null());
    // SAFETY: null asks for the error of the last failed init.
    assert_eq!(text(unsafe { last(ptr::null()) }), "asked to fail");

    // SAFETY: as above.
    assert_eq!(
        unsafe { init(c"not json".as_ptr(), &mut module) },
        OBC_ERR_INVALID
    );
    // SAFETY: as above.
    assert!(text(unsafe { last(ptr::null()) }).starts_with("the configuration is not JSON"));
    // SAFETY: a null configuration is refused.
    assert_eq!(unsafe { init(ptr::null(), &mut module) }, OBC_ERR_INVALID);
    // SAFETY: a null output pointer is refused.
    assert_eq!(
        unsafe { init(c"{}".as_ptr(), ptr::null_mut()) },
        OBC_ERR_INVALID
    );
}

#[test]
fn load_and_process_a_host_nv12_frame() {
    let vt = vtable();
    let module = init(vt, c"{\"step\": 2}");
    let instance = create(vt, module);

    let (status, report) = load(vt, instance);
    assert_eq!(status, OBC_OK);
    assert_eq!(report.struct_size as usize, size_of::<obc_load_report>());
    assert_eq!(report.load_ms, 1.5);
    assert_eq!(report.warmup_ms, -1.0);
    assert_eq!(report.reused_model, 0);
    // A second load is a switch.
    assert_eq!(load(vt, instance).1.reused_model, 1);

    let mut buf = vec![10u8; 4 * 2 + 4];
    let frame = nv12(&mut buf, 4, 2);
    // SAFETY: the frame's memory lives across the call; in place.
    let status = unsafe { vt.instance_process.unwrap()(instance, &frame, &frame, ptr::null(), 0) };
    assert_eq!(
        status,
        OBC_OK,
        "{}",
        text(unsafe { vt.instance_last_error.unwrap()(instance) })
    );
    assert_eq!(&buf[..8], &[12; 8], "luma moved by the step");
    assert_eq!(&buf[8..], &[10; 4], "chroma untouched");

    // A repeat reaches the module as one: this one leaves it as it came.
    let mut repeat = frame;
    repeat.flags = OBC_FRAME_FLAG_REPEAT;
    // SAFETY: as above.
    let status =
        unsafe { vt.instance_process.unwrap()(instance, &repeat, &repeat, ptr::null(), 0) };
    assert_eq!(status, OBC_OK);
    assert_eq!(&buf[..8], &[12; 8], "the repeat was told apart");

    let (mut timings, mut n) = (ptr::null(), 0usize);
    // SAFETY: a live instance.
    assert_eq!(
        unsafe { vt.instance_timings.unwrap()(instance, &mut timings, &mut n) },
        OBC_OK
    );
    assert_eq!(n, 1);
    // SAFETY: `n` entries owned by the instance.
    let t = unsafe { &*timings };
    assert_eq!((text(t.step).as_str(), t.ms), ("test", 0.25));

    let mut dropped = ptr::null();
    // SAFETY: as above.
    assert_eq!(
        unsafe { vt.instance_reset_temporal.unwrap()(instance, &mut dropped) },
        OBC_OK
    );
    assert_eq!(text(dropped), "[\"test\"]");

    // SAFETY: as above.
    assert_eq!(unsafe { vt.instance_park.unwrap()(instance) }, OBC_OK);
    // SAFETY: as above.
    assert_eq!(unsafe { vt.instance_unpark.unwrap()(instance) }, OBC_OK);

    // SAFETY: as above.
    unsafe { vt.instance_unload.unwrap()(instance) };
    // SAFETY: as above.
    let status = unsafe { vt.instance_process.unwrap()(instance, &frame, &frame, ptr::null(), 0) };
    assert_eq!(status, OBC_ERR_STATE, "a frame after unload");

    // SAFETY: destroyed once, the instance before the module.
    unsafe {
        vt.instance_destroy.unwrap()(instance);
        vt.destroy.unwrap()(module);
    }
}

#[test]
fn a_panic_becomes_an_error() {
    let vt = vtable();
    let module = init(vt, c"{}");
    let instance = create(vt, module);
    assert_eq!(load(vt, instance).0, OBC_OK);

    let mut buf = vec![0u8; 12];
    let frame = nv12(&mut buf, 4, 2);
    let live = [obc_param {
        stage: c"test".as_ptr(),
        key: c"panic".as_ptr(),
        value: obc_value::from_bool(true),
    }];
    // SAFETY: everything lives across the call.
    let status =
        unsafe { vt.instance_process.unwrap()(instance, &frame, &frame, live.as_ptr(), 1) };
    assert_eq!(status, OBC_ERR_PANIC);
    // SAFETY: a live instance.
    let reason = text(unsafe { vt.instance_last_error.unwrap()(instance) });
    assert!(reason.contains("boom at frame 0"), "{reason}");

    // The instance still answers.
    // SAFETY: as above.
    let status = unsafe { vt.instance_process.unwrap()(instance, &frame, &frame, ptr::null(), 0) };
    assert_eq!(status, OBC_OK);

    // SAFETY: destroyed once.
    unsafe {
        vt.instance_destroy.unwrap()(instance);
        vt.destroy.unwrap()(module);
    }
}

#[test]
fn null_pointers_are_refused() {
    let _orphan = ORPHAN.lock().unwrap_or_else(|e| e.into_inner());
    let vt = vtable();
    let mut json = ptr::null();
    // SAFETY: a null module is refused.
    assert_eq!(
        unsafe { vt.probe.unwrap()(ptr::null_mut(), &mut json) },
        OBC_ERR_INVALID
    );

    let module = init(vt, c"{}");
    // SAFETY: a null output pointer is refused.
    assert_eq!(
        unsafe { vt.probe.unwrap()(module, ptr::null_mut()) },
        OBC_ERR_INVALID
    );
    // SAFETY: a live module.
    assert!(text(unsafe { vt.last_error.unwrap()(module) }).contains("null"));

    let instance = create(vt, module);
    assert_eq!(load(vt, instance).0, OBC_OK);
    let mut buf = vec![0u8; 12];
    let frame = nv12(&mut buf, 4, 2);
    // SAFETY: a null input is refused.
    let status =
        unsafe { vt.instance_process.unwrap()(instance, ptr::null(), &frame, ptr::null(), 0) };
    assert_eq!(status, OBC_ERR_INVALID);
    // SAFETY: a live instance.
    assert!(
        text(unsafe { vt.instance_last_error.unwrap()(instance) }).contains("input: frame is null")
    );
    // SAFETY: live parameters at a null pointer are refused.
    let status = unsafe { vt.instance_process.unwrap()(instance, &frame, &frame, ptr::null(), 2) };
    assert_eq!(status, OBC_ERR_INVALID);
    // SAFETY: a null request is refused.
    let mut report = obc_load_report::empty();
    assert_eq!(
        unsafe { vt.instance_load.unwrap()(instance, ptr::null(), &mut report) },
        OBC_ERR_INVALID
    );
    // SAFETY: a null instance is refused, and destroying null does nothing.
    unsafe {
        assert_eq!(vt.instance_park.unwrap()(ptr::null_mut()), OBC_ERR_INVALID);
        vt.instance_destroy.unwrap()(ptr::null_mut());
        vt.instance_destroy.unwrap()(instance);
        vt.destroy.unwrap()(module);
    }
}

#[test]
fn small_structures_are_refused() {
    let vt = vtable();
    let module = init(vt, c"{}");
    let instance = create(vt, module);
    assert_eq!(load(vt, instance).0, OBC_OK);

    let mut buf = vec![0u8; 12];
    let mut frame = nv12(&mut buf, 4, 2);
    frame.struct_size = 24;
    // SAFETY: the frame lives across the call.
    let status = unsafe { vt.instance_process.unwrap()(instance, &frame, &frame, ptr::null(), 0) };
    assert_eq!(status, OBC_ERR_INVALID);
    // SAFETY: a live instance.
    let reason = text(unsafe { vt.instance_last_error.unwrap()(instance) });
    assert!(reason.contains("struct_size 24"), "{reason}");
    assert_eq!(buf, vec![0u8; 12], "nothing processed");

    let mut req = obc_chain_request::empty();
    req.struct_size = 16;
    let mut report = obc_load_report::empty();
    // SAFETY: as above.
    let status = unsafe { vt.instance_load.unwrap()(instance, &req, &mut report) };
    assert_eq!(status, OBC_ERR_INVALID);
    // SAFETY: as above.
    assert!(
        text(unsafe { vt.instance_last_error.unwrap()(instance) })
            .contains("chain request struct_size 16")
    );

    let req = obc_chain_request::empty();
    report.struct_size = 4;
    // SAFETY: as above.
    let status = unsafe { vt.instance_load.unwrap()(instance, &req, &mut report) };
    assert_eq!(status, OBC_ERR_INVALID);
    // SAFETY: as above.
    assert!(
        text(unsafe { vt.instance_last_error.unwrap()(instance) })
            .contains("load report struct_size 4")
    );

    // A failed load leaves the instance unloaded.
    // SAFETY: as above.
    let frame = nv12(&mut buf, 4, 2);
    let status = unsafe { vt.instance_process.unwrap()(instance, &frame, &frame, ptr::null(), 0) };
    assert_eq!(status, OBC_ERR_STATE);

    // SAFETY: destroyed once.
    unsafe {
        vt.instance_destroy.unwrap()(instance);
        vt.destroy.unwrap()(module);
    }
}

#[test]
fn unsupported_requests_fail_with_the_module_reason() {
    let vt = vtable();
    let module = init(vt, c"{}");
    let instance = create(vt, module);
    let mut req = obc_chain_request::empty();
    req.input_kind = OBC_FRAME_CUDA;
    let mut report = obc_load_report::empty();
    // SAFETY: everything lives across the call.
    let status = unsafe { vt.instance_load.unwrap()(instance, &req, &mut report) };
    assert_eq!(status, OBC_ERR_UNSUPPORTED);
    // SAFETY: a live instance.
    assert_eq!(
        text(unsafe { vt.instance_last_error.unwrap()(instance) }),
        "host NV12 only"
    );
    // SAFETY: destroyed once.
    unsafe {
        vt.instance_destroy.unwrap()(instance);
        vt.destroy.unwrap()(module);
    }
}

/// Loads the `test` stage with the boolean load-time parameter `key` set,
/// for the frames of [`load`] (a switch on an instance it loaded).
fn load_with(vt: &obc_module_vtable, instance: *mut obc_instance, key: &CStr) -> obc_status {
    load_sized(vt, instance, key, 4)
}

/// [`load_with`] for frames `width` pixels wide.
fn load_sized(
    vt: &obc_module_vtable,
    instance: *mut obc_instance,
    key: &CStr,
    width: u32,
) -> obc_status {
    let stages = [c"test".as_ptr()];
    let params = [obc_param {
        stage: c"test".as_ptr(),
        key: key.as_ptr(),
        value: obc_value::from_bool(true),
    }];
    let mut req = obc_chain_request::empty();
    req.stages = stages.as_ptr();
    req.n_stages = 1;
    req.input_kind = OBC_FRAME_HOST;
    req.input_format = OBC_FORMAT_NV12;
    req.input_width = width;
    req.input_height = 2;
    req.output_format = OBC_FORMAT_NV12;
    req.output_width = width;
    req.output_height = 2;
    req.matrix = OBC_MATRIX_BT601;
    req.chroma_siting = OBC_CHROMA_SITING_MPEG2;
    req.params = params.as_ptr();
    req.n_params = 1;
    let mut report = obc_load_report::empty();
    // SAFETY: everything lives across the call.
    unsafe { vt.instance_load.unwrap()(instance, &req, &mut report) }
}

#[test]
fn a_stage_failure_names_the_stage_and_keeps_the_chain_loaded() {
    let vt = vtable();
    let module = init(vt, c"{}");
    let instance = create(vt, module);
    assert_eq!(load(vt, instance).0, OBC_OK);

    assert_eq!(load_with(vt, instance, c"fail_stage"), OBC_ERR_STAGE);
    // SAFETY: a live instance.
    assert_eq!(
        text(unsafe { vt.instance_last_error.unwrap()(instance) }),
        "test: the stage could not take it"
    );
    let mut buf = vec![10u8; 12];
    let frame = nv12(&mut buf, 4, 2);
    // SAFETY: the frame lives across the call.
    let status = unsafe { vt.instance_process.unwrap()(instance, &frame, &frame, ptr::null(), 0) };
    assert_eq!(status, OBC_OK, "the previous chain is still loaded");

    // SAFETY: destroyed once.
    unsafe {
        vt.instance_destroy.unwrap()(instance);
        vt.destroy.unwrap()(module);
    }
}

#[test]
fn a_failed_switch_keeps_the_previous_chain_and_other_failures_unload() {
    let vt = vtable();
    let module = init(vt, c"{}");
    let instance = create(vt, module);
    let mut buf = vec![10u8; 12];
    let frame = nv12(&mut buf, 4, 2);
    let process =
        // SAFETY: the frame lives across the call; in place.
        || unsafe { vt.instance_process.unwrap()(instance, &frame, &frame, ptr::null(), 0) };

    // A failed first load leaves nothing loaded.
    assert_eq!(load_with(vt, instance, c"fail"), OBC_ERR_FAILED);
    assert_eq!(process(), OBC_ERR_STATE);

    // A failed switch (the same frames) keeps the previous chain running.
    assert_eq!(load(vt, instance).0, OBC_OK);
    assert_eq!(load_with(vt, instance, c"fail"), OBC_ERR_FAILED);
    assert_eq!(process(), OBC_OK, "the previous chain is still loaded");

    // A failed reload (other frames) unloads.
    assert_eq!(load_sized(vt, instance, c"fail", 8), OBC_ERR_FAILED);
    assert_eq!(process(), OBC_ERR_STATE);

    // So does a load that panics, even a switch.
    assert_eq!(load(vt, instance).0, OBC_OK);
    assert_eq!(load_with(vt, instance, c"panic"), OBC_ERR_PANIC);
    assert_eq!(process(), OBC_ERR_STATE);

    // SAFETY: destroyed once.
    unsafe {
        vt.instance_destroy.unwrap()(instance);
        vt.destroy.unwrap()(module);
    }
}
