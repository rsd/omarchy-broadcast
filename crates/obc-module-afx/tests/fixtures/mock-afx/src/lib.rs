//! A stand-in for the NVIDIA Maxine Audio Effects library
//! (`libnv_audiofx.so`), for the tests of `obc-module-afx`. It contains no
//! NVIDIA code: it exports the `NvAFX_*` entry points the module calls, with
//! the behaviour the module relies on, and its effects are a delay and a
//! gain.
//!
//! * `NvAFX_CreateEffect` opens the effect's feature library by its
//!   unversioned name (`libnv_audiofx_<selector>.so`), from the library
//!   search path or the working directory (this library's run path is
//!   `./`), and fails with "effect not available" when it is not found.
//! * Parameters: `input_sample_rate` (48000; 16000 for `superres`),
//!   `output_sample_rate` (`superres` only, 48000, before the load),
//!   `model_path` (a file that
//!   exists), `effect_version` (before the load only; the `denoiser_v2`
//!   model loads only with version 2), `num_samples_per_input_frame`,
//!   `intensity_ratio` (refused by the studio voice effect;
//!   ignored by `dereverb`), `mic_profile_model` (studio voice, needed by
//!   its load) and `mic_profile_id` (studio voice, after the load only).
//!   `supported_num_samples_per_frame` is read before the load only;
//!   `num_samples_per_output_frame` after it.
//! * `NvAFX_Run`: the output is the input delayed by the effect's delay and
//!   scaled by its gain (see below). `superres` takes blocks of 160
//!   samples and gives 480: the input linearly interpolated to three times
//!   its rate (which delays it by two output samples), then delayed by its
//!   delay (in output samples) and scaled by its gain.
//!
//! Each call is written to `mock-afx.log` beside this library (one line
//! each; runs are not logged), and `mock-afx.conf` beside it, read at every
//! call, steers it (`key=value` lines):
//!
//! * `fail_create`, `fail_load`, `fail_run`, `fail_reset`: comma-separated
//!   selectors whose call fails (also from the environment:
//!   `MOCK_AFX_FAIL_CREATE`, `MOCK_AFX_FAIL_LOAD`, `MOCK_AFX_FAIL_RUN`);
//! * `delay.<selector>`, `gain.<selector>`: the effect's delay in samples
//!   and its gain (defaults in [`defaults`]); a mic profile other than
//!   passthrough adds [`PROFILE_DELAY`] samples;
//! * `frames.<selector>`: the block sizes the effect takes (comma-separated);
//!   `out_frame.<selector>`: the output block size it reports;
//!   `no_out_frame.<selector>` (any value): it does not report it.
//!
//! The gain is applied as the effect's strength says: an effect that
//! honours `intensity_ratio` (`denoiser`, `dereverb_denoiser`) scales by
//! `1 - r + r * gain`, the others by `gain`.

#![allow(clippy::missing_safety_doc)]

use std::collections::VecDeque;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io::Write;
use std::path::PathBuf;

const OK: c_int = 0;
const FAILED: c_int = 1;
const INVALID_HANDLE: c_int = 2;
const INVALID_PARAM: c_int = 3;
const IMMUTABLE_PARAM: c_int = 4;
const NOT_AVAILABLE: c_int = 6;
const MODEL_LOAD_FAILED: c_int = 8;
const MODEL_NOT_LOADED: c_int = 9;

/// The delay a mic profile other than passthrough adds.
pub const PROFILE_DELAY: usize = 200;

/// The default delay and gain of `selector`'s effect.
fn defaults(selector: &str) -> (usize, f32) {
    match selector {
        "denoiser" => (700, 0.5),
        "dereverb" => (300, 0.8),
        "dereverb_denoiser" => (500, 0.4),
        "studio_voice_low_latency" => (1100, 0.9),
        "speaker_focus" => (400, 0.7),
        "superres" => (600, 0.6),
        _ => (0, 1.0),
    }
}

const SELECTORS: [&str; 6] = [
    "denoiser",
    "dereverb",
    "dereverb_denoiser",
    "studio_voice_low_latency",
    "speaker_focus",
    "superres",
];

/// The effect that takes 16 kHz and gives 48 kHz.
const SUPERRES: &str = "superres";

unsafe extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dladdr(addr: *const c_void, info: *mut DlInfo) -> c_int;
}

#[repr(C)]
struct DlInfo {
    dli_fname: *const c_char,
    dli_fbase: *mut c_void,
    dli_sname: *const c_char,
    dli_saddr: *mut c_void,
}

const RTLD_NOW: c_int = 2;

/// The directory of this library.
fn here() -> PathBuf {
    let mut info = DlInfo {
        dli_fname: std::ptr::null(),
        dli_fbase: std::ptr::null_mut(),
        dli_sname: std::ptr::null(),
        dli_saddr: std::ptr::null_mut(),
    };
    // SAFETY: the address of a function of this library; `info` writable.
    let ok = unsafe { dladdr(here as fn() -> PathBuf as *const c_void, &mut info) };
    if ok == 0 || info.dli_fname.is_null() {
        return PathBuf::from(".");
    }
    // SAFETY: a NUL-terminated path from the dynamic linker.
    let name = unsafe { CStr::from_ptr(info.dli_fname) }.to_string_lossy();
    PathBuf::from(name.as_ref())
        .parent()
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn log(line: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(here().join("mock-afx.log"))
    {
        let _ = writeln!(f, "{line}");
    }
}

/// The value of `key` in `mock-afx.conf`.
fn conf(key: &str) -> Option<String> {
    let text = std::fs::read_to_string(here().join("mock-afx.conf")).ok()?;
    text.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == key).then(|| v.trim().to_string())
    })
}

/// Whether the configuration (or the environment) makes `call` fail for
/// `selector`.
fn fails(call: &str, selector: &str) -> bool {
    let env = std::env::var(format!("MOCK_AFX_FAIL_{}", call.to_uppercase())).ok();
    [conf(&format!("fail_{call}")), env]
        .into_iter()
        .flatten()
        .any(|list| list.split(',').any(|s| s.trim() == selector))
}

struct Fx {
    selector: String,
    loaded: bool,
    rate: u32,
    out_rate: u32,
    frame: u32,
    model: Option<String>,
    profile_model: Option<String>,
    version: u32,
    intensity: f32,
    profile: i32,
    queue: VecDeque<f32>,
    /// The last input sample, for the interpolation of `superres`.
    prev: f32,
}

impl Fx {
    fn delay(&self) -> usize {
        let base = conf(&format!("delay.{}", self.selector))
            .and_then(|v| v.parse().ok())
            .unwrap_or(defaults(&self.selector).0);
        base + if self.profile >= 0 { PROFILE_DELAY } else { 0 }
    }

    fn gain(&self) -> f32 {
        let g = conf(&format!("gain.{}", self.selector))
            .and_then(|v| v.parse().ok())
            .unwrap_or(defaults(&self.selector).1);
        match self.selector.as_str() {
            "denoiser" | "dereverb_denoiser" => 1.0 - self.intensity + self.intensity * g,
            _ => g,
        }
    }

    fn frames(&self) -> Vec<u32> {
        if let Some(list) = conf(&format!("frames.{}", self.selector)) {
            return list
                .split(',')
                .filter_map(|v| v.trim().parse().ok())
                .collect();
        }
        match self.selector.as_str() {
            "studio_voice_low_latency" => vec![480],
            SUPERRES => vec![160],
            _ => vec![480, 960],
        }
    }

    /// The output block size.
    fn out_frame(&self) -> u32 {
        conf(&format!("out_frame.{}", self.selector))
            .and_then(|v| v.parse().ok())
            .unwrap_or(if self.selector == SUPERRES {
                self.frame * 3
            } else {
                self.frame
            })
    }

    fn when(&self) -> &'static str {
        if self.loaded { "post" } else { "pre" }
    }
}

unsafe fn text<'a>(p: *const c_char) -> &'a str {
    if p.is_null() {
        return "";
    }
    // SAFETY: the caller's NUL-terminated string.
    unsafe { CStr::from_ptr(p) }.to_str().unwrap_or("")
}

unsafe fn fx<'a>(h: *mut c_void) -> Option<&'a mut Fx> {
    // SAFETY: a handle this library made and has not destroyed.
    unsafe { h.cast::<Fx>().as_mut() }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn NvAFX_CreateEffect(
    code: *const c_char,
    effect: *mut *mut c_void,
) -> c_int {
    // SAFETY: the caller's string.
    let selector = unsafe { text(code) }.to_string();
    if effect.is_null() || !SELECTORS.contains(&selector.as_str()) {
        log(&format!("create {selector} unknown"));
        return INVALID_PARAM;
    }
    let lib = CString::new(format!("libnv_audiofx_{selector}.so")).unwrap();
    // SAFETY: a valid name; the library stays loaded.
    let found = unsafe { dlopen(lib.as_ptr(), RTLD_NOW) };
    if found.is_null() || fails("create", &selector) {
        log(&format!("create {selector} not-available"));
        return NOT_AVAILABLE;
    }
    log(&format!("create {selector}"));
    let frame = if selector == SUPERRES { 160 } else { 480 };
    let fx = Box::new(Fx {
        selector,
        loaded: false,
        rate: 0,
        out_rate: 0,
        frame,
        model: None,
        profile_model: None,
        version: 1,
        intensity: 1.0,
        profile: -1,
        queue: VecDeque::new(),
        prev: 0.0,
    });
    // SAFETY: writable by the caller's contract.
    unsafe { *effect = Box::into_raw(fx).cast() };
    OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn NvAFX_DestroyEffect(effect: *mut c_void) -> c_int {
    if effect.is_null() {
        return INVALID_HANDLE;
    }
    // SAFETY: made by `NvAFX_CreateEffect`, destroyed once.
    let fx = unsafe { Box::from_raw(effect.cast::<Fx>()) };
    log(&format!("destroy {}", fx.selector));
    OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn NvAFX_SetU32(effect: *mut c_void, name: *const c_char, val: u32) -> c_int {
    // SAFETY: the caller's handle and string.
    let (Some(fx), name) = (unsafe { fx(effect) }, unsafe { text(name) }) else {
        return INVALID_HANDLE;
    };
    log(&format!(
        "set_u32 {name} {val} {} {}",
        fx.selector,
        fx.when()
    ));
    let sv = fx.selector == "studio_voice_low_latency";
    match name {
        "input_sample_rate" if val == 48_000 || val == 16_000 => fx.rate = val,
        "output_sample_rate" if fx.loaded => return IMMUTABLE_PARAM,
        "output_sample_rate" if fx.selector == SUPERRES && val == 48_000 => fx.out_rate = val,
        "effect_version" if fx.loaded => return IMMUTABLE_PARAM,
        "effect_version" if fx.selector == "denoiser" && (1..=2).contains(&val) => fx.version = val,
        "num_samples_per_input_frame" if fx.loaded => return IMMUTABLE_PARAM,
        "num_samples_per_input_frame" if fx.frames().contains(&val) => fx.frame = val,
        "mic_profile_id" if sv && fx.loaded && (val as i32) >= -1 && (val as i32) <= 5 => {
            fx.profile = val as i32
        }
        _ => return INVALID_PARAM,
    }
    OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn NvAFX_SetFloat(
    effect: *mut c_void,
    name: *const c_char,
    val: f32,
) -> c_int {
    // SAFETY: the caller's handle and string.
    let (Some(fx), name) = (unsafe { fx(effect) }, unsafe { text(name) }) else {
        return INVALID_HANDLE;
    };
    log(&format!(
        "set_f32 {name} {val} {} {}",
        fx.selector,
        fx.when()
    ));
    match name {
        "intensity_ratio"
            if fx.selector != "studio_voice_low_latency" && (0.0..=1.0).contains(&val) =>
        {
            fx.intensity = val
        }
        _ => return INVALID_PARAM,
    }
    OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn NvAFX_SetString(
    effect: *mut c_void,
    name: *const c_char,
    val: *const c_char,
) -> c_int {
    // SAFETY: the caller's handle and strings.
    let (Some(fx), name, val) = (unsafe { fx(effect) }, unsafe { text(name) }, unsafe {
        text(val)
    }) else {
        return INVALID_HANDLE;
    };
    log(&format!(
        "set_str {name} {val} {} {}",
        fx.selector,
        fx.when()
    ));
    if fx.loaded {
        return IMMUTABLE_PARAM;
    }
    match name {
        "model_path" => fx.model = Some(val.to_string()),
        "mic_profile_model" if fx.selector == "studio_voice_low_latency" => {
            fx.profile_model = Some(val.to_string())
        }
        _ => return INVALID_PARAM,
    }
    OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn NvAFX_GetU32(
    effect: *mut c_void,
    name: *const c_char,
    val: *mut u32,
) -> c_int {
    // SAFETY: the caller's handle and string.
    let (Some(fx), name) = (unsafe { fx(effect) }, unsafe { text(name) }) else {
        return INVALID_HANDLE;
    };
    let v = match name {
        "num_samples_per_input_frame" => fx.frame,
        "num_samples_per_output_frame" if fx.loaded && conf(&format!("no_out_frame.{}", fx.selector)).is_none() => {
            fx.out_frame()
        }
        "num_input_channels" | "num_output_channels" => 1,
        _ => return INVALID_PARAM,
    };
    // SAFETY: writable by the caller's contract.
    unsafe { *val = v };
    OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn NvAFX_GetU32List(
    effect: *mut c_void,
    name: *const c_char,
    list: *mut u32,
    size: *mut u32,
) -> c_int {
    // SAFETY: the caller's handle and string.
    let (Some(fx), name) = (unsafe { fx(effect) }, unsafe { text(name) }) else {
        return INVALID_HANDLE;
    };
    if name != "supported_num_samples_per_frame" || size.is_null() {
        return INVALID_PARAM;
    }
    if fx.loaded {
        return IMMUTABLE_PARAM;
    }
    let frames = fx.frames();
    let frames = frames.as_slice();
    // SAFETY: readable and writable by the caller's contract.
    let cap = unsafe { *size } as usize;
    // SAFETY: as above.
    unsafe { *size = frames.len() as u32 };
    if cap < frames.len() || list.is_null() {
        return 7;
    }
    for (i, f) in frames.iter().enumerate() {
        // SAFETY: `cap` writable entries.
        unsafe { *list.add(i) = *f };
    }
    OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn NvAFX_Load(effect: *mut c_void) -> c_int {
    // SAFETY: the caller's handle.
    let Some(fx) = (unsafe { fx(effect) }) else {
        return INVALID_HANDLE;
    };
    let model = fx.model.clone().unwrap_or_default();
    let present = |p: &str| !p.is_empty() && std::path::Path::new(p).is_file();
    let status = if fails("load", &fx.selector) {
        FAILED
    } else if fx.rate == 0 || !present(&model) {
        MODEL_LOAD_FAILED
    } else if fx.selector == SUPERRES && (fx.rate != 16_000 || fx.out_rate != 48_000) {
        INVALID_PARAM
    } else if model.contains("denoiser_v2") && fx.version != 2 {
        FAILED
    } else if fx.selector == "studio_voice_low_latency"
        && !fx.profile_model.as_deref().is_some_and(present)
    {
        MODEL_LOAD_FAILED
    } else {
        OK
    };
    log(&format!("load {} {status}", fx.selector));
    if status == OK {
        fx.loaded = true;
    }
    status
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn NvAFX_Run(
    effect: *mut c_void,
    input: *const *const f32,
    output: *mut *mut f32,
    n: u32,
    channels: u32,
) -> c_int {
    // SAFETY: the caller's handle.
    let Some(fx) = (unsafe { fx(effect) }) else {
        return INVALID_HANDLE;
    };
    if !fx.loaded {
        return MODEL_NOT_LOADED;
    }
    if n != fx.frame || channels != 1 || input.is_null() || output.is_null() {
        return INVALID_PARAM;
    }
    if fails("run", &fx.selector) {
        log(&format!("run {} failed", fx.selector));
        return FAILED;
    }
    let out_n = fx.out_frame() as usize;
    // SAFETY: one channel of `n` samples in and of the output frame out,
    // by the caller's contract.
    let (src, dst) = unsafe {
        (
            std::slice::from_raw_parts(*input, n as usize),
            std::slice::from_raw_parts_mut(*output, out_n),
        )
    };
    let delay = fx.delay();
    let gain = fx.gain();
    while fx.queue.len() < delay {
        fx.queue.push_front(0.0);
    }
    if fx.selector == SUPERRES {
        for (o, s) in dst.chunks_mut(3).zip(src) {
            for (k, d) in o.iter_mut().enumerate() {
                let t = (k + 1) as f32 / 3.0;
                fx.queue.push_back(fx.prev + (s - fx.prev) * t);
                *d = fx.queue.pop_front().unwrap_or(0.0) * gain;
            }
            fx.prev = *s;
        }
        return OK;
    }
    for (d, s) in dst.iter_mut().zip(src) {
        fx.queue.push_back(*s);
        *d = fx.queue.pop_front().unwrap_or(0.0) * gain;
    }
    OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn NvAFX_Reset(
    effect: *mut c_void,
    bitmap: *mut c_char,
    length: c_int,
) -> c_int {
    // SAFETY: the caller's handle.
    let Some(fx) = (unsafe { fx(effect) }) else {
        return INVALID_HANDLE;
    };
    if bitmap.is_null() || length < 1 {
        return INVALID_PARAM;
    }
    if fails("reset", &fx.selector) {
        return FAILED;
    }
    log(&format!("reset {}", fx.selector));
    fx.queue.clear();
    fx.prev = 0.0;
    OK
}
