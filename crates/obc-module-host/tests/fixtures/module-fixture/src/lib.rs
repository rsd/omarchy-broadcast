//! A test module for `obc-module-host`. Its behaviour is chosen by its
//! configuration and by parameters, so that one library exercises every
//! path of the host:
//!
//! * configuration (`init`): `fail_init` (a failing init), `panic_probe` (a
//!   probe that panics), `unavailable` (a stage id reported unavailable,
//!   "Fixture says no"), `omit` (a stage id left out of the report),
//!   `extra` (a stage the manifest does not declare, reported),
//!   `bad_settings` (invalid settings among valid ones), `check_fails` (the
//!   deeper check refuses every stage, with this reason);
//! * every `init` logs `fixture-init n=<count>`, the count of inits since
//!   the library was first loaded (a process-wide static);
//! * stages: `alpha` adds `alpha.step` (load-time, default 10) to every luma
//!   byte of a host NV12 frame; `beta`, `background` and `enhance` do
//!   nothing to the pixels. CUDA frames are never touched;
//! * load-time parameters, each failing the load before anything of it is
//!   in force: `beta.fail_load` as a stage failure of `beta`,
//!   `background.fail_load` as one of `background`, `alpha.blame` (a
//!   string) as one of the stage it names, `alpha.fail_load` as a failure
//!   of the chain (`OBC_ERR_FAILED`); live parameters `alpha.panic`: the
//!   frame panics, `beta.fail`: the frame fails as a stage failure of
//!   `beta`, which is then dropped, `background.fail`: likewise for
//!   `background`;
//! * a temporal reset drops `beta`;
//! * every load and frame is logged at info level (`fixture-load ...`,
//!   `fixture-process ...`), and every settings description
//!   (`fixture-settings`), so a test can read what crossed the ABI.
//!
//! With a feature, the library exports another ABI version (or refuses the
//! host) by hand, and its entry point aborts the process if it is called.
//! With `abi-1-0` it is the same module, exported as one of ABI 1.0 (its
//! version and its vtable say 1.0), as a module built before ABI 1.1 is.
//!
//! The process log line has `repeat=<bool>`, whether the frame carried
//! `OBC_FRAME_FLAG_REPEAT`; the frame is processed either way. With
//! `abi-1-1` it is exported as one of ABI 1.1, its vtable of the size of
//! 1.1 (without the audio entries of 1.2), and with `abi-1-2` as one of
//! ABI 1.2, its vtable of the size of 1.2 (without the status values of
//! 1.3).
//!
//! While `alpha` is loaded, the status values (ABI 1.3) report
//! `alpha`/`step`, its step, and `alpha`/`frames`, the frames processed
//! since the load; none otherwise.
//!
//! With `raw-status-values` it is the same module of ABI 1.3, but its
//! `instance_status_values` ignores the instance and answers call `k`
//! (counted from 0 since the library was loaded) with shape `k % 6`:
//!
//! 0. `OBC_ERR_FAILED`, with one valid entry written out;
//! 1. `OBC_OK` with a null list and a count of 3;
//! 2. `OBC_OK` with a list misaligned for `obc_status_value`, of 1;
//! 3. `OBC_OK` with 65 valid entries (`alpha`/`v<i>`, `i`);
//! 4. `OBC_OK` with 64 valid entries, as above;
//! 5. `OBC_OK` with, in order: `alpha`/`zoom` 1.5; `alpha`/`nan` NaN;
//!    `alpha`/`inf` infinity; `alpha` with an empty name; `alpha` with a
//!    null name; a null stage; the stage `Not a stage`; `alpha` with a name
//!    of 40 `ü` (80 bytes); `alpha` with a name of 63 `a` and one `ü`
//!    (65 bytes), each 2.0.
//!
//! With `audio` it is an `audio-filter` module of ABI 1.2 instead, for the
//! audio worker's tests. Its probe reports the audio stages `mic-denoise`,
//! `mic-dereverb` and `mic-enhance` (the configuration's `unavailable` and
//! `omit` apply), its settings are a `fixture` group of the `mic` surface,
//! and its instances, loaded with `audio_instance_load`:
//!
//! * copy the input to the output, channel by channel (an output channel
//!   past the input's last takes the last), times the gain of
//!   `mic-enhance` when it is in the chain: `mic-enhance.gain_pct`
//!   (load-time and live, an int percentage, 100 by default);
//! * report a voice probability of 0.5 while `mic-denoise` is in the
//!   chain, none otherwise, and a latency of 10 samples per stage (the
//!   audio is not delayed);
//! * take the proposed block size, or the load-time
//!   `mic-denoise.frame_samples` (an int) when given;
//! * fail the load as a stage failure of `<stage>` with the load-time
//!   `<stage>.fail_load`, or with `OBC_FIXTURE_AUDIO_FAIL_LOAD=<stage>`
//!   (`chain`: a failure of the whole chain, `OBC_ERR_FAILED`);
//! * fail a block as a stage failure of `<stage>`, which is dropped, with
//!   the live `<stage>.fail`; with `OBC_FIXTURE_AUDIO_FAIL_AT=<n>`, block
//!   `n` fails as a stage failure of the chain's first stage;
//! * panic on the live `mic-denoise.panic`;
//! * abort the process on block `n` with `OBC_FIXTURE_AUDIO_ABORT_AT=<n>`
//!   (a crash, for the worker's crash ledger);
//! * sleep `OBC_FIXTURE_AUDIO_SLEEP_MS` milliseconds in every block (an
//!   overrun, for the worker's fail-open counter).
//!
//! Blocks count from 1 since the last successful audio load; the
//! environment is read at each audio load. Every audio load is logged at
//! info level (`fixture-audio-load ...`) and every block at debug level
//! (`fixture-audio-process n=<block> ...`).

#[cfg(not(any(
    feature = "abi-major-2",
    feature = "abi-minor-newer",
    feature = "refuse-host"
)))]
mod module {
    use obc_module_sdk::{
        Apply, ChainRequest, Error, Frame, FrameKind, Group, Instance, LoadReport, Module, Op,
        Param, PixelFormat, ProbeReport, Role, Setting, SettingsDescription, StatusValue, Value,
        log, serde_json,
    };

    #[cfg(not(feature = "audio"))]
    const STAGES: [&str; 5] = ["alpha", "beta", "gamma", "background", "enhance"];
    #[cfg(feature = "audio")]
    const STAGES: [&str; 3] = ["mic-denoise", "mic-dereverb", "mic-enhance"];
    /// The stages a video load accepts.
    const VIDEO_STAGES: [&str; 5] = ["alpha", "beta", "gamma", "background", "enhance"];

    /// The inits since the library was first loaded.
    static INITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    #[derive(Default)]
    pub struct Fixture {
        panic_probe: bool,
        unavailable: Option<String>,
        omit: Option<String>,
        extra: bool,
        bad_settings: bool,
        check_fails: Option<String>,
    }

    fn text(v: &Value<'_>) -> String {
        match v {
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => format!("{f}"),
            Value::Str(s) => (*s).to_string(),
        }
    }

    fn params(p: &[Param<'_>]) -> String {
        p.iter()
            .map(|p| format!("{}.{}={}", p.stage, p.key, text(&p.value)))
            .collect::<Vec<_>>()
            .join(";")
    }

    fn frame(f: &Frame<'_>) -> String {
        let kind = f.kind().name().unwrap_or("?");
        let format = f.format().name().unwrap_or("?");
        let cuda = if f.cuda().is_null() { "no" } else { "yes" };
        format!(
            "{kind}:{format}:{}x{}:{}:cuda={cuda}",
            f.width(),
            f.height(),
            f.planes().len()
        )
    }

    impl Module for Fixture {
        type Instance = FixtureInstance;

        fn init(config: serde_json::Value) -> Result<Self, Error> {
            let flag = |k: &str| config.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
            let text = |k: &str| config.get(k).and_then(|v| v.as_str()).map(str::to_string);
            let n = INITS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            log::info!("fixture-init n={n}");
            if flag("fail_init") {
                return Err(Error::unavailable("the fixture was told to fail"));
            }
            Ok(Fixture {
                panic_probe: flag("panic_probe"),
                unavailable: text("unavailable"),
                omit: text("omit"),
                extra: flag("extra"),
                bad_settings: flag("bad_settings"),
                check_fails: text("check_fails"),
            })
        }

        fn check(&self, stages: &[&str]) -> Result<(), Error> {
            log::info!("fixture-check stages={}", stages.join(","));
            match &self.check_fails {
                Some(why) => Err(Error::unavailable(why.clone())),
                None => Ok(()),
            }
        }

        fn probe(&self) -> ProbeReport {
            if self.panic_probe {
                panic!("the probe was told to panic");
            }
            let mut r = ProbeReport::new();
            for s in STAGES {
                if self.omit.as_deref() == Some(s) {
                    continue;
                }
                r = if self.unavailable.as_deref() == Some(s) {
                    r.unavailable(s, "Fixture says no")
                } else {
                    r.available(s)
                };
            }
            if self.extra {
                r = r.available("zeta");
            }
            r
        }

        fn settings(&self) -> SettingsDescription {
            log::info!("fixture-settings");
            if cfg!(feature = "audio") {
                return SettingsDescription::new().group(
                    Group::new("fixture", "Fixture")
                        .surface("mic")
                        .setting(
                            Setting::bool("mic.fixture.denoise", "Denoise", true)
                                .stage("mic-denoise")
                                .role(Role::Enable),
                        )
                        .setting(
                            Setting::int("mic.fixture.gain", "Gain", 0, 400, 25, 100)
                                .unit("%")
                                .stage("mic-enhance")
                                .key("gain_pct")
                                .apply(Apply::Live),
                        ),
                );
            }
            let mut g = Group::new("fixture", "Fixture")
                .setting(
                    Setting::bool("effect.fixture.alpha", "Alpha", false)
                        .stage("alpha")
                        .role(Role::Enable),
                )
                .setting(
                    Setting::int("effect.fixture.step", "Step", 0, 50, 5, 10)
                        .stage("alpha")
                        .key("step")
                        .apply(Apply::Reload)
                        .enabled_when("effect.fixture.alpha", Op::Eq, true)
                        .disabled_reason("Turn on alpha to change this"),
                );
            if self.bad_settings {
                g = g
                    .setting(Setting::bool("effect.fixture.Bad", "Bad id", false))
                    .setting(Setting::bool("effect.fixture.nope", "Nope", false).stage("nope"));
            }
            SettingsDescription::new().group(g)
        }

        fn create_instance(&self) -> Result<FixtureInstance, Error> {
            Ok(FixtureInstance::default())
        }
    }

    #[derive(Default)]
    pub struct FixtureInstance {
        stages: Vec<String>,
        step: u8,
        /// Frames processed since the last load (ABI 1.3 status values).
        frames: u32,
        /// The audio side.
        #[cfg_attr(not(feature = "audio"), allow(dead_code))]
        audio: AudioState,
    }

    /// What an audio load set.
    #[derive(Default)]
    #[cfg_attr(not(feature = "audio"), allow(dead_code))]
    struct AudioState {
        gain: f32,
        blocks: u64,
        abort_at: Option<u64>,
        fail_at: Option<u64>,
        sleep_ms: u64,
    }

    impl Instance for FixtureInstance {
        fn load(&mut self, req: &ChainRequest<'_>) -> Result<LoadReport, Error> {
            let bg = req.background().map(|f| {
                // SAFETY: the background is a host frame owned by the host
                // for the call; only read.
                let first = unsafe { f.host_plane(0) }
                    .ok()
                    .and_then(|p| p.first().copied())
                    .unwrap_or(0);
                format!("{}:{first}", frame(f))
            });
            log::info!(
                "fixture-load stages={} kind={} in={}:{}x{} out={}:{}x{} matrix={:?} siting={:?} \
                 full={} cuda={} params={} bg={}",
                req.stages().join(","),
                req.input_kind().name().unwrap_or("?"),
                req.input_format().name().unwrap_or("?"),
                req.input_size().0,
                req.input_size().1,
                req.output_format().name().unwrap_or("?"),
                req.output_size().0,
                req.output_size().1,
                req.matrix(),
                req.chroma_siting(),
                req.full_range(),
                if req.cuda().is_null() { "no" } else { "yes" },
                params(req.params()),
                bg.unwrap_or_else(|| "none".into()),
            );
            if let Some(other) = req.stages().iter().find(|s| !VIDEO_STAGES.contains(s)) {
                return Err(Error::invalid(format!("unknown stage {other:?}")));
            }
            let told = |stage: &str, key: &str| {
                req.param(stage, key).and_then(|v| v.as_bool()) == Some(true)
            };
            if told("beta", "fail_load") {
                return Err(Error::stage("beta", "beta was told to fail its load"));
            }
            if told("background", "fail_load") {
                return Err(Error::stage(
                    "background",
                    "background was told to fail its load",
                ));
            }
            if let Some(stage) = req.param("alpha", "blame").and_then(|v| v.as_str()) {
                return Err(Error::stage(stage, "alpha blamed it"));
            }
            if told("alpha", "fail_load") {
                return Err(Error::failed("alpha was told to fail its load"));
            }
            let reused = !self.stages.is_empty();
            self.stages = req.stages().iter().map(|s| s.to_string()).collect();
            self.frames = 0;
            self.step = req
                .param("alpha", "step")
                .and_then(|v| v.as_int())
                .map_or(10, |s| s.clamp(0, 255) as u8);
            Ok(LoadReport {
                load_ms: 1.5,
                warmup_ms: Some(0.5),
                reused_model: reused,
            })
        }

        fn process(
            &mut self,
            input: &Frame<'_>,
            output: &Frame<'_>,
            live: &[Param<'_>],
        ) -> Result<(), Error> {
            log::info!(
                "fixture-process in={} out={} same={} repeat={} live={}",
                frame(input),
                frame(output),
                input.same_memory(output),
                input.is_repeat(),
                params(live)
            );
            if Param::find(live, "alpha", "panic") == Some(Value::Bool(true)) {
                panic!("alpha was told to panic");
            }
            for stage in ["beta", "background"] {
                if self.stages.iter().any(|s| s == stage)
                    && Param::find(live, stage, "fail") == Some(Value::Bool(true))
                {
                    self.stages.retain(|s| s != stage);
                    return Err(Error::stage(stage, format!("{stage} was told to fail")));
                }
            }
            if self.stages.iter().any(|s| s == "alpha")
                && input.kind() == FrameKind::Host
                && input.format() == PixelFormat::Nv12
                && input.same_memory(output)
            {
                // SAFETY: the host owns the frame for the call, in place.
                let y = unsafe { output.host_plane_mut(0) }?;
                let (rows, row_bytes) = output.plane_geometry(0).unwrap_or((0, 0));
                let pitch = output.plane(0).map_or(0, |p| p.pitch as usize);
                for r in 0..rows {
                    for b in &mut y[r * pitch..r * pitch + row_bytes] {
                        *b = b.saturating_add(self.step);
                    }
                }
            }
            self.frames += 1;
            Ok(())
        }

        fn reset_temporal(&mut self) -> Result<Vec<String>, Error> {
            let had = self.stages.iter().any(|s| s == "beta");
            self.stages.retain(|s| s != "beta");
            Ok(if had { vec!["beta".into()] } else { Vec::new() })
        }

        fn unload(&mut self) {
            self.stages.clear();
            self.audio = AudioState::default();
        }

        fn status_values(&self) -> Vec<StatusValue> {
            if !self.stages.iter().any(|s| s == "alpha") {
                return Vec::new();
            }
            vec![
                StatusValue::new("alpha", "step", f64::from(self.step)),
                StatusValue::new("alpha", "frames", f64::from(self.frames)),
            ]
        }

        fn timings(&self) -> Vec<(String, f32)> {
            let mut t: Vec<(String, f32)> = self
                .stages
                .iter()
                .enumerate()
                .map(|(i, s)| (s.clone(), i as f32 + 1.0))
                .collect();
            if self.stages.iter().any(|s| s == "enhance") {
                t.insert(0, ("convert-in".into(), 0.25));
                t.push(("convert-out".into(), 0.75));
            }
            t
        }
    }

    #[cfg(feature = "audio")]
    mod audio {
        use super::{AudioState, FixtureInstance, params};
        use obc_module_sdk::{
            AudioBuffer, AudioBufferMut, AudioInstance, AudioLoadReport, AudioRequest, Error,
            Param, log,
        };

        fn env_number(name: &str) -> Option<u64> {
            std::env::var(name).ok().and_then(|v| v.trim().parse().ok())
        }

        fn gain_of(v: Option<obc_module_sdk::Value<'_>>) -> Option<f32> {
            v.and_then(|v| v.as_int())
                .map(|p| p.clamp(0, 1000) as f32 / 100.0)
        }

        impl AudioInstance for FixtureInstance {
            fn load_audio(&mut self, req: &AudioRequest<'_>) -> Result<AudioLoadReport, Error> {
                log::info!(
                    "fixture-audio-load stages={} rate={} in={} out={} frame={} params={}",
                    req.stages().join(","),
                    req.sample_rate(),
                    req.channels_in(),
                    req.channels_out(),
                    req.frame_samples(),
                    params(req.params()),
                );
                if let Some(other) = req.stages().iter().find(|s| !super::STAGES.contains(s)) {
                    return Err(Error::invalid(format!("unknown stage {other:?}")));
                }
                let env_fail = std::env::var("OBC_FIXTURE_AUDIO_FAIL_LOAD").ok();
                if env_fail.as_deref() == Some("chain") {
                    return Err(Error::failed("the audio chain was told to fail its load"));
                }
                for stage in req.stages() {
                    let told = req.param(stage, "fail_load").and_then(|v| v.as_bool())
                        == Some(true)
                        || env_fail.as_deref() == Some(*stage);
                    if told {
                        return Err(Error::stage(
                            *stage,
                            format!("{stage} was told to fail its load"),
                        ));
                    }
                }
                let frame_samples = req
                    .param("mic-denoise", "frame_samples")
                    .and_then(|v| v.as_int())
                    .map(|n| n as u32);
                let reused = !self.stages.is_empty();
                self.stages = req.stages().iter().map(|s| s.to_string()).collect();
                self.audio = AudioState {
                    gain: gain_of(req.param("mic-enhance", "gain_pct")).unwrap_or(1.0),
                    blocks: 0,
                    abort_at: env_number("OBC_FIXTURE_AUDIO_ABORT_AT"),
                    fail_at: env_number("OBC_FIXTURE_AUDIO_FAIL_AT"),
                    sleep_ms: env_number("OBC_FIXTURE_AUDIO_SLEEP_MS").unwrap_or(0),
                };
                Ok(AudioLoadReport {
                    load_ms: 2.0,
                    warmup_ms: None,
                    frame_samples,
                    latency_samples: 10 * req.stages().len() as u32,
                    reused_model: reused,
                })
            }

            fn process_audio(
                &mut self,
                input: &AudioBuffer<'_>,
                output: &mut AudioBufferMut<'_>,
                live: &[Param<'_>],
            ) -> Result<(), Error> {
                let a = &mut self.audio;
                a.blocks += 1;
                log::debug!(
                    "fixture-audio-process n={} flags={} pts={:?} live={}",
                    a.blocks,
                    input.flags(),
                    input.pts_ns(),
                    params(live)
                );
                if a.abort_at == Some(a.blocks) {
                    log::error!("fixture-audio-abort n={}", a.blocks);
                    std::process::abort();
                }
                if a.sleep_ms > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(a.sleep_ms));
                }
                if Param::find(live, "mic-denoise", "panic").and_then(|v| v.as_bool()) == Some(true)
                {
                    panic!("mic-denoise was told to panic");
                }
                let failing = self
                    .stages
                    .iter()
                    .find(|s| Param::find(live, s, "fail").and_then(|v| v.as_bool()) == Some(true))
                    .cloned()
                    .or_else(|| {
                        (a.fail_at == Some(a.blocks))
                            .then(|| self.stages.first().cloned())
                            .flatten()
                    });
                if let Some(stage) = failing {
                    self.stages.retain(|s| *s != stage);
                    return Err(Error::stage(
                        stage.clone(),
                        format!("{stage} was told to fail"),
                    ));
                }
                if let Some(g) = gain_of(Param::find(live, "mic-enhance", "gain_pct")) {
                    a.gain = g;
                }
                let gain = if self.stages.iter().any(|s| s == "mic-enhance") {
                    a.gain
                } else {
                    1.0
                };
                let last = input.channels() - 1;
                for c in 0..output.channels() {
                    let src = input.plane(c.min(last)).unwrap_or_default();
                    if let Some(dst) = output.plane_mut(c) {
                        for (d, s) in dst.iter_mut().zip(src) {
                            *d = s * gain;
                        }
                    }
                }
                let denoise = self.stages.iter().any(|s| s == "mic-denoise");
                output.set_voice_probability(denoise.then_some(0.5));
                Ok(())
            }
        }
    }

    #[cfg(not(any(
        feature = "abi-1-0",
        feature = "abi-1-1",
        feature = "abi-1-2",
        feature = "raw-status-values",
        feature = "audio"
    )))]
    obc_module_sdk::export_module!(Fixture);

    /// The exports of `export_module!`, with `instance_status_values`
    /// replaced by [`raw_status_values::values`].
    #[cfg(feature = "raw-status-values")]
    mod raw_status_values {
        use std::ffi::{CString, c_char};
        use std::sync::atomic::{AtomicUsize, Ordering};

        use obc_module_sdk::abi::{
            OBC_ERR_FAILED, OBC_OK, obc_host, obc_instance, obc_module_vtable, obc_status,
            obc_status_value,
        };

        /// The calls so far.
        static CALLS: AtomicUsize = AtomicUsize::new(0);

        /// An entry; `stage` and `name` are leaked (the fixture's lists
        /// are few and small) or null.
        fn entry(stage: Option<&str>, name: Option<&str>, value: f64) -> obc_status_value {
            let leak = |s: Option<&str>| -> *const c_char {
                s.map_or(std::ptr::null(), |s| {
                    CString::new(s).unwrap().into_raw().cast_const()
                })
            };
            obc_status_value {
                stage: leak(stage),
                name: leak(name),
                value,
                reserved0: 0,
                reserved1: 0,
            }
        }

        /// `n` valid entries, leaked.
        fn valid(n: usize) -> &'static [obc_status_value] {
            let v: Vec<_> = (0..n)
                .map(|i| entry(Some("alpha"), Some(&format!("v{i}")), i as f64))
                .collect();
            Vec::leak(v)
        }

        /// The shapes described in the crate's documentation.
        ///
        /// # Safety
        ///
        /// `out` and `n` are writable.
        unsafe extern "C" fn values(
            _instance: *mut obc_instance,
            out: *mut *const obc_status_value,
            n: *mut usize,
        ) -> obc_status {
            let (status, list, count): (obc_status, *const obc_status_value, usize) =
                match CALLS.fetch_add(1, Ordering::SeqCst) % 6 {
                    0 => (OBC_ERR_FAILED, valid(1).as_ptr(), 1),
                    1 => (OBC_OK, std::ptr::null(), 3),
                    2 => {
                        let base = valid(2).as_ptr().cast::<u8>();
                        // SAFETY: within the leaked list of two entries.
                        (OBC_OK, unsafe { base.add(4) }.cast(), 1)
                    }
                    3 => (OBC_OK, valid(65).as_ptr(), 65),
                    4 => (OBC_OK, valid(64).as_ptr(), 64),
                    _ => {
                        let long_a = format!("{}ü", "a".repeat(63));
                        let long_u = "ü".repeat(40);
                        let v = vec![
                            entry(Some("alpha"), Some("zoom"), 1.5),
                            entry(Some("alpha"), Some("nan"), f64::NAN),
                            entry(Some("alpha"), Some("inf"), f64::INFINITY),
                            entry(Some("alpha"), Some(""), 2.0),
                            entry(Some("alpha"), None, 2.0),
                            entry(None, Some("nostage"), 2.0),
                            entry(Some("Not a stage"), Some("bad"), 2.0),
                            entry(Some("alpha"), Some(&long_u), 2.0),
                            entry(Some("alpha"), Some(&long_a), 2.0),
                        ];
                        let v = Vec::leak(v);
                        (OBC_OK, v.as_ptr(), v.len())
                    }
                };
            // SAFETY: by the caller's contract.
            unsafe {
                *out = list;
                *n = count;
            }
            status
        }

        /// The SDK's entry point, with the raw `instance_status_values`.
        ///
        /// # Safety
        ///
        /// As `export_module!`'s: `host` is null or valid until unload.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn obc_module_entry(
            host: *const obc_host,
        ) -> *const obc_module_vtable {
            static VTABLE: obc_module_vtable = {
                let mut v = obc_module_sdk::ffi::vtable::<super::Fixture>();
                v.instance_status_values = Some(values);
                v
            };
            // SAFETY: the caller's contract is the one `ffi::entry` needs.
            unsafe { obc_module_sdk::ffi::entry(host, &VTABLE) }
        }

        /// The ABI this crate's SDK describes.
        #[unsafe(no_mangle)]
        pub extern "C" fn obc_module_abi_version() -> u32 {
            obc_module_sdk::abi::OBC_MODULE_ABI_VERSION
        }
    }

    #[cfg(feature = "audio")]
    obc_module_sdk::export_module!(Fixture, audio);

    /// The exports of `export_module!`, claiming ABI 1.0 (or 1.1, or 1.2),
    /// with a vtable of that version's size: it ends before the audio
    /// entries of 1.2 (1.0 and 1.1), or before the status values of 1.3
    /// (1.2).
    #[cfg(any(feature = "abi-1-0", feature = "abi-1-1", feature = "abi-1-2"))]
    mod abi_1_0 {
        use obc_module_sdk::abi::{obc_host, obc_module_abi_pack, obc_module_vtable};

        /// The minor version claimed.
        const MINOR: u16 = if cfg!(feature = "abi-1-0") {
            0
        } else if cfg!(feature = "abi-1-1") {
            1
        } else {
            2
        };

        /// The size of the vtable of that version.
        const SIZE: u32 = if MINOR < 2 {
            std::mem::offset_of!(obc_module_vtable, audio_instance_load) as u32
        } else {
            std::mem::offset_of!(obc_module_vtable, instance_status_values) as u32
        };

        /// ABI 1.0, 1.1 or 1.2.
        #[unsafe(no_mangle)]
        pub extern "C" fn obc_module_abi_version() -> u32 {
            obc_module_abi_pack(1, MINOR)
        }

        /// The SDK's entry point, with a vtable that says 1.0, 1.1 or 1.2.
        ///
        /// # Safety
        ///
        /// As `export_module!`'s: `host` is null or valid until unload.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn obc_module_entry(
            host: *const obc_host,
        ) -> *const obc_module_vtable {
            static VTABLE: obc_module_vtable = {
                let mut v = obc_module_sdk::ffi::vtable::<super::Fixture>();
                v.abi_version = obc_module_abi_pack(1, MINOR);
                v.struct_size = SIZE;
                v
            };
            // SAFETY: the caller's contract is the one `ffi::entry` needs.
            unsafe { obc_module_sdk::ffi::entry(host, &VTABLE) }
        }
    }
}

#[cfg(any(
    feature = "abi-major-2",
    feature = "abi-minor-newer",
    feature = "refuse-host"
))]
mod handwritten {
    use obc_module_sdk::abi::{obc_host, obc_module_abi_pack, obc_module_vtable};

    /// The ABI version this variant claims.
    #[unsafe(no_mangle)]
    pub extern "C" fn obc_module_abi_version() -> u32 {
        if cfg!(feature = "abi-major-2") {
            obc_module_abi_pack(2, 0)
        } else if cfg!(feature = "abi-minor-newer") {
            obc_module_abi_pack(1, 99)
        } else {
            obc_module_abi_pack(1, 0)
        }
    }

    /// Refuses the host; for the other versions, a host must never get
    /// here, and the process ends if it does.
    #[unsafe(no_mangle)]
    pub extern "C" fn obc_module_entry(_host: *const obc_host) -> *const obc_module_vtable {
        if cfg!(feature = "refuse-host") {
            return std::ptr::null();
        }
        std::process::abort();
    }
}
