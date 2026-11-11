//! Drives an audio module exported with `export_module!(_, audio)` exactly
//! as the audio worker does: through the exported symbols and the vtable's
//! function pointers, with raw ABI structures (ABI 1.2).

use std::ffi::{CStr, c_char};
use std::ptr;

use obc_module_sdk::abi::*;
use obc_module_sdk::{
    AudioBuffer, AudioBufferMut, AudioInstance, AudioLoadReport, AudioRequest, Error, Instance,
    Module, Param, ProbeReport, SettingsDescription, Value,
};

// ---------------------------------------------------------------------------
// The module under test
// ---------------------------------------------------------------------------

/// `mic-gain` multiplies every sample by its `gain` (load-time, live
/// override); it takes 480- or 960-sample blocks and chooses 960 when
/// asked by the load parameter `big`. Load parameters `fail`, `fail_stage`
/// and `panic` fail the load; the live parameter `fail` fails a block as a
/// stage failure.
struct AudioModule;

struct AudioOnly {
    gain: f32,
    loads: u32,
    dropped: bool,
}

impl Module for AudioModule {
    type Instance = AudioOnly;

    fn init(_config: obc_module_sdk::serde_json::Value) -> Result<Self, Error> {
        Ok(AudioModule)
    }

    fn probe(&self) -> ProbeReport {
        ProbeReport::new().available("mic-gain")
    }

    fn settings(&self) -> SettingsDescription {
        SettingsDescription::new()
    }

    fn create_instance(&self) -> Result<AudioOnly, Error> {
        Ok(AudioOnly {
            gain: 1.0,
            loads: 0,
            dropped: false,
        })
    }
}

/// An audio-only instance: the video entries refuse.
impl Instance for AudioOnly {
    obc_module_sdk::no_video_stages!();

    fn unload(&mut self) {
        self.loads = 0;
    }
}

impl AudioInstance for AudioOnly {
    fn load_audio(&mut self, req: &AudioRequest<'_>) -> Result<AudioLoadReport, Error> {
        if req.stages() != ["mic-gain"] {
            return Err(Error::invalid("only mic-gain"));
        }
        let told = |k: &str| req.param("mic-gain", k) == Some(Value::Bool(true));
        if told("fail_stage") {
            return Err(Error::stage("mic-gain", "the gain could not take it"));
        }
        if told("fail") {
            return Err(Error::failed("the chain could not load"));
        }
        if told("panic") {
            panic!("the audio load was told to panic");
        }
        self.gain = req
            .param("mic-gain", "gain")
            .and_then(|v| v.as_float())
            .unwrap_or(1.0) as f32;
        self.dropped = false;
        let reused = self.loads > 0;
        self.loads += 1;
        Ok(AudioLoadReport {
            load_ms: 2.5,
            warmup_ms: Some(0.5),
            frame_samples: if told("zero") {
                Some(0)
            } else {
                told("big").then_some(960)
            },
            latency_samples: 120,
            reused_model: reused,
        })
    }

    fn process_audio(
        &mut self,
        input: &AudioBuffer<'_>,
        output: &mut AudioBufferMut<'_>,
        live: &[Param<'_>],
    ) -> Result<(), Error> {
        if Param::find(live, "mic-gain", "fail") == Some(Value::Bool(true)) {
            self.dropped = true;
            return Err(Error::stage("mic-gain", "the gain was told to fail"));
        }
        if let Some(g) = Param::find(live, "mic-gain", "gain").and_then(|v| v.as_float()) {
            self.gain = g as f32;
        }
        let gain = if self.dropped { 1.0 } else { self.gain };
        for c in 0..output.channels() {
            let src = input.plane(c.min(input.channels() - 1)).unwrap();
            let dst = output.plane_mut(c).unwrap();
            for (d, s) in dst.iter_mut().zip(src) {
                *d = s * gain;
            }
        }
        output.set_voice_probability(Some(0.75));
        Ok(())
    }
}

obc_module_sdk::export_module!(AudioModule, audio);

// ---------------------------------------------------------------------------
// A host
// ---------------------------------------------------------------------------

fn vtable() -> &'static obc_module_vtable {
    // A leaked host without a log callback.
    let host: &'static obc_host = Box::leak(Box::new(obc_host::empty()));
    // SAFETY: the host lives forever.
    let vt = unsafe { obc_module_entry(host) };
    assert!(!vt.is_null());
    // SAFETY: the vtable is static.
    unsafe { &*vt }
}

fn text(ptr: *const c_char) -> String {
    assert!(!ptr.is_null());
    // SAFETY: the module returns NUL-terminated strings.
    unsafe { CStr::from_ptr(ptr) }.to_str().unwrap().to_string()
}

/// A module object and one instance of it.
fn instance(vt: &obc_module_vtable) -> (*mut obc_module, *mut obc_instance) {
    let mut module = ptr::null_mut();
    // SAFETY: valid arguments.
    assert_eq!(
        unsafe { vt.init.unwrap()(c"{}".as_ptr(), &mut module) },
        OBC_OK
    );
    let mut instance = ptr::null_mut();
    // SAFETY: a live module.
    assert_eq!(
        unsafe { vt.instance_create.unwrap()(module, &mut instance) },
        OBC_OK
    );
    (module, instance)
}

fn destroy(vt: &obc_module_vtable, (module, instance): (*mut obc_module, *mut obc_instance)) {
    // SAFETY: destroyed once, the instance first.
    unsafe {
        vt.instance_destroy.unwrap()(instance);
        vt.destroy.unwrap()(module);
    }
}

/// Loads `mic-gain` at `rate`, mono, 480-sample blocks, with the
/// parameters `params` (`(key, value)`).
fn load(
    vt: &obc_module_vtable,
    instance: *mut obc_instance,
    rate: u32,
    params: &[(&CStr, obc_value)],
) -> (obc_status, obc_audio_load_report) {
    let stages = [c"mic-gain".as_ptr()];
    let params: Vec<obc_param> = params
        .iter()
        .map(|(k, v)| obc_param {
            stage: c"mic-gain".as_ptr(),
            key: k.as_ptr(),
            value: *v,
        })
        .collect();
    let mut req = obc_audio_request::empty();
    req.stages = stages.as_ptr();
    req.n_stages = 1;
    req.sample_rate = rate;
    req.channels_in = 1;
    req.channels_out = 1;
    req.frame_samples = 480;
    req.params = if params.is_empty() {
        ptr::null()
    } else {
        params.as_ptr()
    };
    req.n_params = params.len();
    let mut report = obc_audio_load_report::empty(req.frame_samples);
    // SAFETY: everything lives across the call.
    let status = unsafe { vt.audio_instance_load.unwrap()(instance, &req, &mut report) };
    (status, report)
}

fn mono(plane: &mut [f32], rate: u32) -> obc_audio_buffer {
    let mut b = obc_audio_buffer::empty();
    b.format = OBC_AUDIO_F32_PLANAR;
    b.sample_rate = rate;
    b.channels = 1;
    b.n_samples = plane.len() as u32;
    b.planes[0] = plane.as_mut_ptr();
    b
}

/// Runs one block of `n` samples of 0.25 at 48 kHz; the status, the output
/// and the voice probability.
fn process(
    vt: &obc_module_vtable,
    instance: *mut obc_instance,
    n: usize,
    live: &[obc_param],
) -> (obc_status, Vec<f32>, f32) {
    let mut a = vec![0.25f32; n];
    let mut z = vec![0.0f32; n];
    let input = mono(&mut a, 48_000);
    let mut output = mono(&mut z, 48_000);
    // SAFETY: the buffers, their planes and the parameters live across
    // the call.
    let status = unsafe {
        vt.audio_instance_process.unwrap()(
            instance,
            &input,
            &mut output,
            if live.is_empty() {
                ptr::null()
            } else {
                live.as_ptr()
            },
            live.len(),
        )
    };
    (status, z, output.voice_probability)
}

fn last_error(vt: &obc_module_vtable, instance: *mut obc_instance) -> String {
    // SAFETY: a live instance.
    text(unsafe { vt.instance_last_error.unwrap()(instance) })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn the_audio_form_of_the_macro_fills_the_audio_entries() {
    assert_eq!(obc_module_abi_version(), obc_module_abi_pack(1, 2));
    let vt = vtable();
    assert_eq!(vt.struct_size as usize, size_of::<obc_module_vtable>());
    assert!(vt.audio_instance_load.is_some() && vt.audio_instance_process.is_some());
    // The video entries stay set, as the ABI requires.
    assert!(vt.instance_load.is_some() && vt.instance_process.is_some());
}

#[test]
fn a_block_is_processed_with_the_load_time_and_live_gain() {
    let vt = vtable();
    let obj = instance(vt);
    let (_, inst) = obj;
    assert_eq!(
        process(vt, inst, 480, &[]).0,
        OBC_ERR_STATE,
        "before a load"
    );
    assert!(last_error(vt, inst).contains("before a successful audio load"));

    let (status, report) = load(vt, inst, 48_000, &[(c"gain", obc_value::from_float(2.0))]);
    assert_eq!(status, OBC_OK, "{}", last_error(vt, inst));
    assert_eq!(
        report.struct_size as usize,
        size_of::<obc_audio_load_report>()
    );
    assert_eq!(
        (
            report.load_ms,
            report.warmup_ms,
            report.frame_samples,
            report.latency_samples,
            report.reused_model
        ),
        (2.5, 0.5, 480, 120, 0)
    );
    let (status, out, voice) = process(vt, inst, 480, &[]);
    assert_eq!(status, OBC_OK, "{}", last_error(vt, inst));
    assert_eq!(out, vec![0.5; 480]);
    assert_eq!(voice, 0.75);

    // A live gain applies from this block on.
    let live = [obc_param {
        stage: c"mic-gain".as_ptr(),
        key: c"gain".as_ptr(),
        value: obc_value::from_float(4.0),
    }];
    assert_eq!(process(vt, inst, 480, &live).1, vec![1.0; 480]);
    assert_eq!(process(vt, inst, 480, &[]).1, vec![1.0; 480]);

    // A block of another size than the load's is refused.
    let (status, ..) = process(vt, inst, 240, &[]);
    assert_eq!(status, OBC_ERR_INVALID);
    assert!(
        last_error(vt, inst).contains("240 samples"),
        "{}",
        last_error(vt, inst)
    );

    // A switch reuses; the module may choose another block size.
    let (status, report) = load(vt, inst, 48_000, &[(c"big", obc_value::from_bool(true))]);
    assert_eq!(status, OBC_OK);
    assert_eq!((report.frame_samples, report.reused_model), (960, 1));
    assert_eq!(process(vt, inst, 480, &[]).0, OBC_ERR_INVALID);
    assert_eq!(process(vt, inst, 960, &[]).0, OBC_OK);

    // Unloaded, it refuses blocks.
    // SAFETY: a live instance.
    unsafe { vt.instance_unload.unwrap()(inst) };
    assert_eq!(process(vt, inst, 960, &[]).0, OBC_ERR_STATE);
    destroy(vt, obj);
}

#[test]
fn an_audio_only_instance_refuses_video() {
    let vt = vtable();
    let obj = instance(vt);
    let (_, inst) = obj;
    let stages = [c"mic-gain".as_ptr()];
    let mut req = obc_chain_request::empty();
    req.stages = stages.as_ptr();
    req.n_stages = 1;
    req.input_kind = OBC_FRAME_HOST;
    req.input_format = OBC_FORMAT_NV12;
    let mut report = obc_load_report::empty();
    // SAFETY: everything lives across the call.
    let status = unsafe { vt.instance_load.unwrap()(inst, &req, &mut report) };
    assert_eq!(status, OBC_ERR_UNSUPPORTED);
    assert_eq!(last_error(vt, inst), "this module has no video stages");

    // A frame after an audio load: no video chain is loaded.
    assert_eq!(load(vt, inst, 48_000, &[]).0, OBC_OK);
    let frame = obc_frame::empty();
    // SAFETY: the frame lives across the call.
    let status = unsafe { vt.instance_process.unwrap()(inst, &frame, &frame, ptr::null(), 0) };
    assert_eq!(status, OBC_ERR_STATE);
    destroy(vt, obj);
}

#[test]
fn a_block_stage_failure_names_the_stage() {
    let vt = vtable();
    let obj = instance(vt);
    let (_, inst) = obj;
    assert_eq!(
        load(vt, inst, 48_000, &[(c"gain", obc_value::from_float(2.0))]).0,
        OBC_OK
    );
    let live = [obc_param {
        stage: c"mic-gain".as_ptr(),
        key: c"fail".as_ptr(),
        value: obc_value::from_bool(true),
    }];
    assert_eq!(process(vt, inst, 480, &live).0, OBC_ERR_STAGE);
    assert_eq!(last_error(vt, inst), "mic-gain: the gain was told to fail");
    // The next block runs without the stage (a pass-through here).
    assert_eq!(process(vt, inst, 480, &[]).1, vec![0.25; 480]);
    destroy(vt, obj);
}

#[test]
fn a_failed_audio_switch_keeps_the_chain_and_other_failures_unload() {
    let vt = vtable();
    let obj = instance(vt);
    let (_, inst) = obj;
    let fail = [(c"fail", obc_value::from_bool(true))];

    // A failed first load leaves nothing loaded.
    assert_eq!(load(vt, inst, 48_000, &fail).0, OBC_ERR_FAILED);
    assert_eq!(process(vt, inst, 480, &[]).0, OBC_ERR_STATE);

    // A failed switch (the same format) keeps the previous chain.
    assert_eq!(load(vt, inst, 48_000, &[]).0, OBC_OK);
    assert_eq!(load(vt, inst, 48_000, &fail).0, OBC_ERR_FAILED);
    assert_eq!(process(vt, inst, 480, &[]).0, OBC_OK);
    let stage = [(c"fail_stage", obc_value::from_bool(true))];
    assert_eq!(load(vt, inst, 48_000, &stage).0, OBC_ERR_STAGE);
    assert_eq!(last_error(vt, inst), "mic-gain: the gain could not take it");
    assert_eq!(process(vt, inst, 480, &[]).0, OBC_OK);

    // A failed reload (another rate) unloads.
    assert_eq!(load(vt, inst, 16_000, &fail).0, OBC_ERR_FAILED);
    assert_eq!(process(vt, inst, 480, &[]).0, OBC_ERR_STATE);

    // So does a load that panics, even a switch.
    assert_eq!(load(vt, inst, 48_000, &[]).0, OBC_OK);
    let panic = [(c"panic", obc_value::from_bool(true))];
    assert_eq!(load(vt, inst, 48_000, &panic).0, OBC_ERR_PANIC);
    assert!(last_error(vt, inst).contains("told to panic"));
    assert_eq!(process(vt, inst, 480, &[]).0, OBC_ERR_STATE);

    // A switch the module made but whose block size of 0 is refused
    // unloads: the module no longer holds the previous chain.
    assert_eq!(load(vt, inst, 48_000, &[]).0, OBC_OK);
    let zero = [(c"zero", obc_value::from_bool(true))];
    assert_eq!(load(vt, inst, 48_000, &zero).0, OBC_ERR_INVALID);
    assert!(last_error(vt, inst).contains("0 samples"));
    assert_eq!(process(vt, inst, 480, &[]).0, OBC_ERR_STATE);
    destroy(vt, obj);
}

#[test]
fn small_or_null_audio_structures_are_refused() {
    let vt = vtable();
    let obj = instance(vt);
    let (_, inst) = obj;
    let mut req = obc_audio_request::empty();
    req.struct_size = 8;
    let mut report = obc_audio_load_report::empty(480);
    // SAFETY: everything lives across the calls.
    unsafe {
        let load = vt.audio_instance_load.unwrap();
        assert_eq!(load(inst, &req, &mut report), OBC_ERR_INVALID);
        assert!(last_error(vt, inst).contains("struct_size 8"));
        assert_eq!(load(inst, ptr::null(), &mut report), OBC_ERR_INVALID);
        let req = obc_audio_request::empty();
        assert_eq!(load(inst, &req, ptr::null_mut()), OBC_ERR_INVALID);
        let mut small = obc_audio_load_report::empty(480);
        small.struct_size = 4;
        assert_eq!(load(inst, &req, &mut small), OBC_ERR_INVALID);
        assert_eq!(load(ptr::null_mut(), &req, &mut report), OBC_ERR_INVALID);
    }
    assert_eq!(load(vt, inst, 48_000, &[]).0, OBC_OK);
    let mut a = vec![0.0f32; 480];
    let input = mono(&mut a, 48_000);
    // SAFETY: as above.
    let status = unsafe {
        vt.audio_instance_process.unwrap()(inst, &input, ptr::null_mut(), ptr::null(), 0)
    };
    assert_eq!(status, OBC_ERR_INVALID);
    destroy(vt, obj);
}
