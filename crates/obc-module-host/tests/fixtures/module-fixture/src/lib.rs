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
//! `OBC_FRAME_FLAG_REPEAT`; the frame is processed either way.

#[cfg(not(any(
    feature = "abi-major-2",
    feature = "abi-minor-newer",
    feature = "refuse-host"
)))]
mod module {
    use obc_module_sdk::{
        Apply, ChainRequest, Error, Frame, FrameKind, Group, Instance, LoadReport, Module, Op,
        Param, PixelFormat, ProbeReport, Role, Setting, SettingsDescription, Value, log,
        serde_json,
    };

    const STAGES: [&str; 5] = ["alpha", "beta", "gamma", "background", "enhance"];

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
            if let Some(other) = req.stages().iter().find(|s| !STAGES.contains(s)) {
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
            Ok(())
        }

        fn reset_temporal(&mut self) -> Result<Vec<String>, Error> {
            let had = self.stages.iter().any(|s| s == "beta");
            self.stages.retain(|s| s != "beta");
            Ok(if had { vec!["beta".into()] } else { Vec::new() })
        }

        fn unload(&mut self) {
            self.stages.clear();
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

    #[cfg(not(feature = "abi-1-0"))]
    obc_module_sdk::export_module!(Fixture);

    /// The exports of `export_module!`, claiming ABI 1.0.
    #[cfg(feature = "abi-1-0")]
    mod abi_1_0 {
        use obc_module_sdk::abi::{obc_host, obc_module_abi_pack, obc_module_vtable};

        /// ABI 1.0.
        #[unsafe(no_mangle)]
        pub extern "C" fn obc_module_abi_version() -> u32 {
            obc_module_abi_pack(1, 0)
        }

        /// The SDK's entry point, with a vtable that says 1.0.
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
                v.abi_version = obc_module_abi_pack(1, 0);
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
