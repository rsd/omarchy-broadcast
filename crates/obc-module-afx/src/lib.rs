//! NVIDIA Maxine Audio Effects as an omarchy-broadcast audio module
//! (`br.com.dias.maxine-afx`, kind `audio-filter`).
//!
//! The library this crate builds (`libobc_module_afx.so`, with the manifest
//! `maxine-afx.toml`) runs microphone effects of the NVIDIA Maxine Audio
//! Effects SDK, which is not part of omarchy-broadcast: it is opened at run
//! time, when a chain is checked or loaded, so the library loads on a
//! machine without it. Only the audio worker opens it.
//!
//! # Stages
//!
//! | Stage | Slot | Runs |
//! |---|---|---|
//! | `mic-focus` | clean (position 25) | speaker focus (`speaker_focus`), when installed |
//! | `mic-dereverb` | clean (position 50) | room echo removal (`dereverb`) |
//! | `mic-denoise` | clean (position 100, the default) | noise removal (`denoiser`, model version 2) |
//! | `mic-bandwidth` | clean (position 150) | headset bandwidth restoration (`superres`, 16 kHz to 48 kHz), when installed; after noise and echo removal |
//! | `mic-enhance` | shape | studio voice, low latency, with a mic profile; conflicts with `mic-denoise` and `mic-dereverb` |
//!
//! All take 48 kHz mono blocks of 480 samples, on an NVIDIA GPU
//! (`mic-bandwidth` decimates them to 16 kHz for its effect, which gives
//! 48 kHz back: [`chain`], [`decimate`]). A chain
//! with room echo and noise removal runs them as one combined model
//! (`dereverb_denoiser`) where it is installed ([`chain`]).
//!
//! # Settings (surface `mic`)
//!
//! * `mic.noise.enabled`, `mic.noise.strength`: noise removal and its
//!   strength (0 to 100 %), the ids every provider of `mic-denoise` shares
//!   (with one apply mode: both providers apply the strength live). The
//!   strength comes with each block and changes the running chain between
//!   two blocks, with no reload and no change of its delay ([`chain`]);
//! * `mic.echo.enabled`, `mic.echo.strength`: room echo removal and its
//!   strength, live as well; with noise removal on, the two run as the
//!   combined model, whose one strength is noise removal's, so this
//!   strength is greyed then;
//! * `mic.voice.enabled`, `mic.voice.profile`: studio voice (which adds a
//!   noticeable delay, said in its label) and its mic profile, which
//!   reloads the chain (it changes the delay). Studio voice already
//!   removes noise and echo: its stage conflicts with `mic-denoise` and `mic-dereverb`, whose
//!   switches are greyed while it is on.
//! * `mic.focus.enabled`: speaker focus, which keeps the main speaker and
//!   removes other voices. It has no strength. It is an Early Access
//!   feature of the SDK, not installed by default: while its feature,
//!   library or models are missing the probe reports its stage
//!   unavailable with the reason "Models not installed"
//!   ([`tree::REASON_MODELS_NOT_INSTALLED`]). It removes neither noise nor
//!   echo, so it conflicts with no other stage.
//! * `mic.bandwidth.enabled`: headset bandwidth restoration, for
//!   narrowband microphones (Bluetooth headsets, 16 kHz): the SDK's
//!   bandwidth extension (`superres`, its 16 kHz to 48 kHz model) rebuilds
//!   the high band the microphone does not capture. It runs after noise
//!   and echo removal, as NVIDIA's own chains do, so that it does not
//!   rebuild the high band from noisy input; it has no strength and
//!   conflicts with no stage. The SDK package does not
//!   install it: while its feature, library or models are missing the
//!   probe reports its stage unavailable with the reason "Models not
//!   installed".
//!
//! The chain's delay ([`AudioLoadReport::latency_samples`]) is measured on
//! the loaded effects ([`latency`]). The voice probability of a block is
//! not reported (see [`chain`]).
//!
//! # Configuration
//!
//! The module's table (`[module."br.com.dias.maxine-afx"]`):
//!
//! * `sdk_root`: the SDK root. The root is found in this order: the
//!   configured `sdk_root` when set and not empty; else the environment's
//!   `MAXINE_AFX_ROOT` when set and not empty; else
//!   `/opt/nvidia/maxine/afx` (the `maxine-afx-sdk-local` package) when it
//!   exists ([`tree::resolve_root`]). The configured root comes first, so
//!   the `MAXINE_AFX_ROOT` the package's environment file sets does not
//!   hide it;
//! * `_host.compute_capability`: `[major, minor]` of the GPU, when the host
//!   knows it, so the probe looks for that GPU's models; absent or null,
//!   models of any GPU count, and a load picks the only model directory
//!   there is or asks the CUDA driver which GPU this is.
//!
//! # Probe and check
//!
//! The probe reads the SDK tree, `/proc/driver/nvidia` and the
//! process's `LD_LIBRARY_PATH` only: the core library, each effect's
//! feature library and its models for this GPU ([`tree::unavailable`]),
//! and whether the library search path leads to the feature library
//! ([`ffi::search_path`]: the SDK finds it there, so a worker started
//! without the environment of the SDK package `maxine-afx-sdk-local`,
//! which the worker applies to itself when it starts, has the stages
//! unavailable, with that reason). The check opens the
//! SDK's core library (once per process) and makes the same check of the
//! feature libraries, creating no CUDA context. The GPU is used from
//! `audio_instance_load` on only, and every effect is destroyed on unload.

use std::path::{Path, PathBuf};
use std::time::Instant;

use obc_module_sdk::{
    Apply, AudioBuffer, AudioBufferMut, AudioInstance, AudioLoadReport, AudioRequest, Error, Group,
    Instance, Module, Op, Param, ProbeReport, Role, Setting, SettingsDescription, Value, log,
    serde_json,
};

pub mod chain;
pub mod decimate;
pub mod ffi;
pub mod latency;
pub mod tree;

use chain::{Chain, FRAME, Params, RATE, Strengths};
use tree::Effect as Kind;

/// The module id.
pub const MODULE_ID: &str = "br.com.dias.maxine-afx";

/// The noise removal stage.
pub const DENOISE: &str = "mic-denoise";
/// The room echo removal stage.
pub const DEREVERB: &str = "mic-dereverb";
/// The studio voice stage.
pub const ENHANCE: &str = "mic-enhance";
/// The speaker focus stage.
pub const FOCUS: &str = "mic-focus";
/// The headset bandwidth restoration stage.
pub const BANDWIDTH: &str = "mic-bandwidth";

/// The probe's reason when the SDK's core is not there.
pub const REASON_NO_SDK: &str = "Maxine Audio Effects SDK not installed";
/// The probe's reason when the NVIDIA driver is not loaded.
pub const REASON_NO_DRIVER: &str = "NVIDIA driver not loaded";

/// The probe's reason when the library search path does not lead to an
/// effect's feature library.
pub const REASON_NO_SEARCH_PATH: &str = "The audio worker's LD_LIBRARY_PATH does not lead to \
     the Maxine feature libraries of the SDK root in use: for the default root, install the \
     maxine-afx-sdk-local package (the worker applies its environment when it starts and logs \
     it, unless OBC_NO_SDK_ENV is set); for another SDK tree, start the worker with \
     LD_LIBRARY_PATH naming that tree's feature libraries";

/// The parameter key of a strength, in percent.
pub const STRENGTH_KEY: &str = "strength";
/// The parameter key of the mic profile.
pub const PROFILE_KEY: &str = "profile";
/// A strength when none is given, in percent.
pub const DEFAULT_STRENGTH: u8 = 100;

/// Noise removal's switch (shared with every provider of the stage).
pub const NOISE_ENABLED: &str = "mic.noise.enabled";
/// Noise removal's strength (shared with every provider of the stage).
pub const NOISE_STRENGTH: &str = "mic.noise.strength";
/// Room echo removal's switch.
pub const ECHO_ENABLED: &str = "mic.echo.enabled";
/// Room echo removal's strength.
pub const ECHO_STRENGTH: &str = "mic.echo.strength";
/// Studio voice's switch.
pub const VOICE_ENABLED: &str = "mic.voice.enabled";
/// Studio voice's mic profile.
pub const VOICE_PROFILE: &str = "mic.voice.profile";
/// Speaker focus's switch.
pub const FOCUS_ENABLED: &str = "mic.focus.enabled";
/// Speaker focus's label, which says what it does.
pub const FOCUS_LABEL: &str = "Speaker focus (removes other voices)";
/// Headset bandwidth restoration's switch.
pub const BANDWIDTH_ENABLED: &str = "mic.bandwidth.enabled";
/// Headset bandwidth restoration's label, which says when it helps.
pub const BANDWIDTH_LABEL: &str = "Restore headset audio (for narrowband mics)";

/// Why noise and room echo removal are greyed while studio voice is on.
pub const REASON_VOICE_ON: &str = "Studio voice already removes noise and echo";
/// Why noise removal's strength is greyed while it is off.
pub const REASON_NOISE_OFF: &str = "Turn on noise removal to change this";
/// Why room echo removal's strength is greyed (off, or combined with noise
/// removal).
pub const REASON_ECHO_STRENGTH: &str = "Set with noise removal's strength while both are on; turn on room echo removal alone to change this";
/// Why the mic profile is greyed while studio voice is off.
pub const REASON_VOICE_OFF: &str = "Turn on studio voice to change this";
/// Studio voice's label, which says what it costs.
pub const VOICE_LABEL: &str = "Studio voice (adds noticeable delay)";

/// A mic profile of the studio voice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Profile {
    /// No profile (the SDK's passthrough).
    #[default]
    Passthrough,
    /// The `bright` profile.
    Bright,
    /// The `vibrant` profile.
    Vibrant,
    /// The `flat` profile.
    Flat,
    /// The `full` profile.
    Full,
    /// The `thin` profile.
    Thin,
    /// The `warm` profile.
    Warm,
}

impl Profile {
    /// Every profile, in display order.
    pub const ALL: [Profile; 7] = [
        Profile::Passthrough,
        Profile::Bright,
        Profile::Vibrant,
        Profile::Flat,
        Profile::Full,
        Profile::Thin,
        Profile::Warm,
    ];

    /// The setting's value id.
    pub fn id(self) -> &'static str {
        match self {
            Profile::Passthrough => "passthrough",
            Profile::Bright => "bright",
            Profile::Vibrant => "vibrant",
            Profile::Flat => "flat",
            Profile::Full => "full",
            Profile::Thin => "thin",
            Profile::Warm => "warm",
        }
    }

    /// The label shown.
    pub fn label(self) -> &'static str {
        match self {
            Profile::Passthrough => "None",
            Profile::Bright => "Bright",
            Profile::Vibrant => "Vibrant",
            Profile::Flat => "Flat",
            Profile::Full => "Full",
            Profile::Thin => "Thin",
            Profile::Warm => "Warm",
        }
    }

    /// The SDK's mic profile id (-1 for none).
    pub fn sdk_id(self) -> i32 {
        Profile::ALL.iter().position(|p| *p == self).unwrap_or(0) as i32 - 1
    }

    /// The profile of value id `id`.
    pub fn from_id(id: &str) -> Option<Profile> {
        Profile::ALL.into_iter().find(|p| p.id() == id)
    }
}

/// The settings description.
pub fn settings() -> SettingsDescription {
    SettingsDescription::new()
        .group(
            Group::new("noise", "Noise removal")
                .surface("mic")
                .setting(
                    Setting::bool(NOISE_ENABLED, "Noise removal", true)
                        .stage(DENOISE)
                        .role(Role::Enable)
                        .enabled_when(VOICE_ENABLED, Op::Eq, false)
                        .disabled_reason(REASON_VOICE_ON)
                        .hint("switch"),
                )
                .setting(
                    Setting::int(
                        NOISE_STRENGTH,
                        "Strength",
                        0,
                        100,
                        1,
                        i64::from(DEFAULT_STRENGTH),
                    )
                    .unit("%")
                    .stage(DENOISE)
                    .key(STRENGTH_KEY)
                    .apply(Apply::Live)
                    .enabled_when(NOISE_ENABLED, Op::Eq, true)
                    .disabled_reason(REASON_NOISE_OFF)
                    .hint("slider"),
                ),
        )
        .group(
            Group::new("echo", "Room echo removal")
                .surface("mic")
                .setting(
                    Setting::bool(ECHO_ENABLED, "Room echo removal", false)
                        .stage(DEREVERB)
                        .role(Role::Enable)
                        .enabled_when(VOICE_ENABLED, Op::Eq, false)
                        .disabled_reason(REASON_VOICE_ON)
                        .hint("switch"),
                )
                .setting(
                    Setting::int(
                        ECHO_STRENGTH,
                        "Strength",
                        0,
                        100,
                        1,
                        i64::from(DEFAULT_STRENGTH),
                    )
                    .unit("%")
                    .stage(DEREVERB)
                    .key(STRENGTH_KEY)
                    .apply(Apply::Live)
                    .enabled_when(ECHO_ENABLED, Op::Eq, true)
                    .enabled_when(NOISE_ENABLED, Op::Eq, false)
                    .disabled_reason(REASON_ECHO_STRENGTH)
                    .hint("slider"),
                ),
        )
        .group(
            Group::new("focus", "Speaker focus").surface("mic").setting(
                Setting::bool(FOCUS_ENABLED, FOCUS_LABEL, false)
                    .stage(FOCUS)
                    .role(Role::Enable)
                    .hint("switch"),
            ),
        )
        .group(
            Group::new("bandwidth", "Headset bandwidth")
                .surface("mic")
                .setting(
                    Setting::bool(BANDWIDTH_ENABLED, BANDWIDTH_LABEL, false)
                        .stage(BANDWIDTH)
                        .role(Role::Enable)
                        .hint("switch"),
                ),
        )
        .group(
            Group::new("voice", "Studio voice")
                .surface("mic")
                .setting(
                    Setting::bool(VOICE_ENABLED, VOICE_LABEL, false)
                        .stage(ENHANCE)
                        .role(Role::Enable)
                        .hint("switch"),
                )
                .setting(
                    Setting::enumeration(
                        VOICE_PROFILE,
                        "Mic profile",
                        Profile::ALL.map(|p| (p.id(), p.label())),
                        Profile::default().id(),
                    )
                    .stage(ENHANCE)
                    .key(PROFILE_KEY)
                    .apply(Apply::Reload)
                    .enabled_when(VOICE_ENABLED, Op::Eq, true)
                    .disabled_reason(REASON_VOICE_OFF),
                ),
        )
}

/// The effect a stage needs installed.
fn stage_effect(stage: &str) -> Kind {
    match stage {
        DEREVERB => Kind::Dereverb,
        ENHANCE => Kind::StudioVoice,
        FOCUS => Kind::SpeakerFocus,
        BANDWIDTH => Kind::SuperRes,
        _ => Kind::Denoiser,
    }
}

/// Why `effect` cannot run from the SDK tree at `root` on a GPU of
/// `compute_capability` (`None`: any), with the NVIDIA driver loaded or
/// not, in a process of library search path `ld_library_path`; `None` when
/// it can. Reads the tree only.
pub fn unavailable(
    root: Option<&Path>,
    compute_capability: Option<(u32, u32)>,
    driver: bool,
    ld_library_path: Option<&str>,
    effect: Kind,
) -> Option<String> {
    if let Some(why) = tree::unavailable(root, compute_capability, driver, effect) {
        return Some(why);
    }
    let root = root?;
    let name = ffi::feature_library(effect.selector());
    ffi::search_path(&tree::library_dir(root, effect), &name, ld_library_path)
        .err()
        .map(|why| {
            log::debug!("{}: {why}", effect.selector());
            REASON_NO_SEARCH_PATH.to_string()
        })
}

/// The probe of the SDK tree at `root` for a GPU of `compute_capability`
/// (`None`: any), with the NVIDIA driver loaded or not, in a process of
/// library search path `ld_library_path`: each stage in manifest order.
pub fn probe_root(
    root: Option<&Path>,
    compute_capability: Option<(u32, u32)>,
    driver: bool,
    ld_library_path: Option<&str>,
) -> ProbeReport {
    let mut report = ProbeReport::new();
    for stage in [BANDWIDTH, FOCUS, DEREVERB, DENOISE, ENHANCE] {
        let why = unavailable(
            root,
            compute_capability,
            driver,
            ld_library_path,
            stage_effect(stage),
        );
        report = match why {
            None => report.available(stage),
            Some(why) => report.unavailable(stage, why),
        };
    }
    report
}

/// The process's library search path.
fn ld_library_path() -> Option<String> {
    std::env::var(ffi::ENV).ok()
}

/// The module's configuration: `sdk_root` and `_host.compute_capability`.
#[derive(Debug, Default, PartialEq)]
struct Config {
    sdk_root: Option<PathBuf>,
    compute_capability: Option<(u32, u32)>,
}

impl Config {
    fn parse(v: &serde_json::Value) -> Result<Config, Error> {
        let Some(obj) = v.as_object() else {
            return if v.is_null() {
                Ok(Config::default())
            } else {
                Err(Error::invalid("the module configuration is not a table"))
            };
        };
        let mut c = Config::default();
        for (k, v) in obj {
            match k.as_str() {
                "sdk_root" => {
                    let s = v
                        .as_str()
                        .ok_or_else(|| Error::invalid("sdk_root is not a string"))?;
                    c.sdk_root = Some(PathBuf::from(s));
                }
                "_host" => {
                    let cc = v.get("compute_capability").filter(|x| !x.is_null());
                    if let Some(cc) = cc {
                        let pair = cc
                            .as_array()
                            .filter(|a| a.len() == 2)
                            .and_then(|a| Some((a[0].as_u64()? as u32, a[1].as_u64()? as u32)))
                            .ok_or_else(|| {
                                Error::invalid("_host.compute_capability is not [major, minor]")
                            })?;
                        c.compute_capability = Some(pair);
                    }
                }
                other => {
                    return Err(Error::invalid(format!(
                        "unknown key {other:?} in the module configuration (known: sdk_root)"
                    )));
                }
            }
        }
        Ok(c)
    }
}

/// The module object.
pub struct AfxModule {
    root: Option<PathBuf>,
    compute_capability: Option<(u32, u32)>,
}

impl AfxModule {
    /// The SDK root the module uses.
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }
}

/// The SDK at `root`, opened (no CUDA context).
fn sdk(root: Option<&Path>) -> Result<&'static ffi::Sdk, Error> {
    let root = root.ok_or_else(|| Error::unavailable(REASON_NO_SDK))?;
    if ffi::core_library(root).is_none() {
        return Err(Error::unavailable(format!(
            "{REASON_NO_SDK} (nothing in {})",
            root.join(ffi::CORE_DIR).display()
        )));
    }
    ffi::open(root).map_err(Error::unavailable)
}

impl Module for AfxModule {
    type Instance = AfxInstance;

    fn init(config: serde_json::Value) -> Result<Self, Error> {
        let config = Config::parse(&config)?;
        let env = std::env::var_os(tree::ROOT_ENV);
        let root = tree::resolve_root(
            config.sdk_root.as_deref(),
            env.as_deref(),
            Path::new(tree::DEFAULT_ROOT),
        );
        match &root {
            Some(r) => log::debug!("the Maxine Audio Effects SDK root is {}", r.display()),
            None => log::debug!("no Maxine Audio Effects SDK root"),
        }
        Ok(AfxModule {
            root,
            compute_capability: config.compute_capability,
        })
    }

    fn probe(&self) -> ProbeReport {
        probe_root(
            self.root.as_deref(),
            self.compute_capability,
            tree::driver_loaded(),
            ld_library_path().as_deref(),
        )
    }

    fn settings(&self) -> SettingsDescription {
        settings()
    }

    /// Opens the SDK's core library and makes sure the library search path
    /// leads to the feature library of each effect `stages` need. No CUDA
    /// context, no model.
    fn check(&self, stages: &[&str]) -> Result<(), Error> {
        let mut kinds: Vec<Kind> = Vec::new();
        for s in stages {
            let id =
                chain::stage_id(s).ok_or_else(|| Error::invalid(format!("unknown stage {s:?}")))?;
            kinds.push(stage_effect(id));
        }
        if kinds.is_empty() {
            return Ok(());
        }
        let sdk = sdk(self.root.as_deref())?;
        let ld = ld_library_path();
        for k in kinds {
            let name = ffi::feature_library(k.selector());
            ffi::search_path(&tree::library_dir(sdk.root(), k), &name, ld.as_deref())
                .map_err(|why| Error::unavailable(format!("Maxine {}: {why}", k.label())))?;
        }
        Ok(())
    }

    fn create_instance(&self) -> Result<AfxInstance, Error> {
        Ok(AfxInstance {
            root: self.root.clone(),
            compute_capability: self.compute_capability,
            chain: None,
            live: Strengths::default(),
        })
    }
}

/// The strength in percent in `value` (an int or a float), clamped to 0 to
/// 100.
fn strength_of(value: Option<Value<'_>>) -> Option<u8> {
    let v = value?;
    let pct = v.as_int().map(|i| i as f64).or_else(|| v.as_float())?;
    if pct.is_nan() {
        return None;
    }
    Some(pct.clamp(0.0, 100.0).round() as u8)
}

/// The strength `stage` asks for in `params` (`None`: not given), 0 to 1.
fn strength_in(params: &[Param<'_>], stage: &str) -> Option<f32> {
    strength_of(Param::find(params, stage, STRENGTH_KEY)).map(|p| f32::from(p) / 100.0)
}

/// One chain of the module's stages.
pub struct AfxInstance {
    root: Option<PathBuf>,
    compute_capability: Option<(u32, u32)>,
    chain: Option<Chain>,
    /// The strengths the blocks last came with, for the next load.
    live: Strengths,
}

impl AfxInstance {
    /// What `req` asks besides its stages. The strengths are live (they
    /// come with each block); a request may give them too, and else the
    /// last live ones (or full strength) are loaded.
    fn params(&self, req: &AudioRequest<'_>) -> Result<Params, Error> {
        let full = f32::from(DEFAULT_STRENGTH) / 100.0;
        let pct = |stage, live: Option<f32>| {
            strength_of(req.param(stage, STRENGTH_KEY))
                .map(|p| f32::from(p) / 100.0)
                .or(live)
                .unwrap_or(full)
        };
        let profile = match req.param(ENHANCE, PROFILE_KEY) {
            None => Profile::default(),
            Some(v) => v.as_str().and_then(Profile::from_id).ok_or_else(|| {
                Error::stage(ENHANCE, format!("{PROFILE_KEY} {v:?} is not a mic profile"))
            })?,
        };
        Ok(Params {
            noise: pct(DENOISE, self.live.noise),
            echo: pct(DEREVERB, self.live.echo),
            profile,
        })
    }
}

/// An audio-only module: the video entry points refuse video.
impl Instance for AfxInstance {
    obc_module_sdk::no_video_stages!();

    /// Forgets every effect's state; returns the stages of the effects
    /// that failed and were not reported yet (they pass their input
    /// through, delayed).
    fn reset_temporal(&mut self) -> Result<Vec<String>, Error> {
        let Some(c) = &mut self.chain else {
            return Ok(Vec::new());
        };
        c.reset();
        Ok(c.take_failures()
            .into_iter()
            .filter_map(|e| e.failed_stage().map(str::to_string))
            .collect())
    }

    /// Destroys every effect, the failed ones included (and with them what
    /// the SDK holds on the GPU).
    fn unload(&mut self) {
        self.chain = None;
    }

    fn timings(&self) -> Vec<(String, f32)> {
        self.chain.as_ref().map(Chain::timings).unwrap_or_default()
    }
}

impl AudioInstance for AfxInstance {
    fn load_audio(&mut self, req: &AudioRequest<'_>) -> Result<AudioLoadReport, Error> {
        let start = Instant::now();
        if req.channels_in() != 1 || req.channels_out() != 1 {
            return Err(Error::unsupported(format!(
                "the Maxine audio effects are mono only: {} channels in, {} out",
                req.channels_in(),
                req.channels_out()
            )));
        }
        if req.sample_rate() != RATE || req.frame_samples() as usize != FRAME {
            return Err(Error::unsupported(format!(
                "the Maxine audio effects run at {RATE} Hz in blocks of {FRAME} samples, not \
                 {} Hz and {}",
                req.sample_rate(),
                req.frame_samples()
            )));
        }
        let sdk = sdk(self.root.as_deref())?;
        let root = sdk.root().to_path_buf();
        let combined = unavailable(
            Some(&root),
            self.compute_capability,
            true,
            ld_library_path().as_deref(),
            Kind::DereverbDenoiser,
        )
        .is_none();
        let params = self.params(req)?;
        let mut plans = chain::plan(req.stages(), params, combined)?;
        if plans.is_empty() {
            return Err(Error::invalid("no stage in the request"));
        }
        // A switch to the same effects keeps them: only what can change on
        // a loaded effect changes.
        let kinds: Vec<Kind> = plans.iter().map(|p| p.kind).collect();
        if let Some(c) = &mut self.chain
            && c.kinds() == kinds
            && c.is_intact()
        {
            for (r, p) in c.effects.iter_mut().zip(&plans) {
                if let Err(e) = r.apply(p, false) {
                    // Half switched: the chain is not the one in force any
                    // more, nor the one asked for.
                    self.chain = None;
                    return Err(e);
                }
            }
            let load_ms = start.elapsed().as_secs_f32() * 1e3;
            log::info!(
                "{}: switched in place, {} samples of delay",
                c.stages().join(" + "),
                c.latency()
            );
            return Ok(AudioLoadReport::new(load_ms)
                .with_latency_samples(c.latency())
                .with_reused_model(true));
        }
        // Built aside: a failed switch keeps the chain in force.
        let mut effects = Vec::new();
        let mut warmup = 0.0f32;
        let mut i = 0;
        while i < plans.len() {
            let t = Instant::now();
            match chain::build(sdk, &root, self.compute_capability, &plans[i]) {
                Ok(r) => effects.push(r),
                Err(e) if plans[i].kind == Kind::DereverbDenoiser => {
                    // The two effects instead of the combined one.
                    log::warn!(
                        "{}: {}; running {} as two effects",
                        plans[i].stages.join(" + "),
                        e.message(),
                        plans[i].kind.label()
                    );
                    let split = chain::plan(&plans[i].stages, params, false)?;
                    plans.splice(i..=i, split);
                    continue;
                }
                Err(e) => return Err(e),
            }
            warmup += t.elapsed().as_secs_f32() * 1e3;
            i += 1;
        }
        let chain = Chain::new(effects);
        let latency = chain.latency();
        let load_ms = start.elapsed().as_secs_f32() * 1e3;
        log::info!(
            "{}: {} samples of delay (loaded in {load_ms:.0} ms)",
            chain
                .effects
                .iter()
                .map(|r| r.plan.kind.selector())
                .collect::<Vec<_>>()
                .join(" + "),
            latency
        );
        self.chain = Some(chain);
        Ok(AudioLoadReport::new(load_ms)
            .with_warmup_ms(warmup.min(load_ms))
            .with_latency_samples(latency))
    }

    fn process_audio(
        &mut self,
        input: &AudioBuffer<'_>,
        output: &mut AudioBufferMut<'_>,
        live: &[Param<'_>],
    ) -> Result<(), Error> {
        let Some(chain) = &mut self.chain else {
            return Err(Error::state("no Maxine audio chain is loaded"));
        };
        // The strengths are live: they come with each block.
        let s = Strengths {
            noise: strength_in(live, DENOISE),
            echo: strength_in(live, DEREVERB),
        };
        self.live = Strengths {
            noise: s.noise.or(self.live.noise),
            echo: s.echo.or(self.live.echo),
        };
        if input.is_new_source() {
            chain.reset();
        }
        let src = input.plane(0).unwrap_or_default();
        let dst = output.plane_mut(0).unwrap_or_default();
        // A chain whose effects all failed passes the blocks through,
        // delayed as it reported.
        chain.process(src, dst, s)
    }
}

obc_module_sdk::export_module!(AfxModule, audio);

#[cfg(test)]
mod tests {
    use super::*;
    use obc_module_sdk::serde_json::json;

    #[test]
    fn the_configuration_is_read_and_unknown_keys_refused() {
        assert_eq!(Config::parse(&json!(null)).unwrap(), Config::default());
        let c = Config::parse(&json!({
            "sdk_root": "/opt/x", "_host": {"compute_capability": [12, 0]}
        }))
        .unwrap();
        assert_eq!(c.sdk_root.as_deref(), Some(Path::new("/opt/x")));
        assert_eq!(c.compute_capability, Some((12, 0)));
        let c = Config::parse(&json!({"_host": {"compute_capability": null}})).unwrap();
        assert_eq!(c.compute_capability, None);
        assert!(Config::parse(&json!({"sdk-root": "/x"})).is_err());
        assert!(Config::parse(&json!({"sdk_root": 3})).is_err());
        assert!(Config::parse(&json!({"_host": {"compute_capability": [12]}})).is_err());
        assert!(Config::parse(&json!([])).is_err());
    }

    #[test]
    fn profiles_map_to_the_sdk_s_ids() {
        let ids: Vec<i32> = Profile::ALL.iter().map(|p| p.sdk_id()).collect();
        assert_eq!(ids, [-1, 0, 1, 2, 3, 4, 5]);
        assert_eq!(Profile::from_id("warm"), Some(Profile::Warm));
        assert_eq!(Profile::from_id("loud"), None);
        assert_eq!(Profile::Passthrough.sdk_id() as u32, u32::MAX);
    }

    #[test]
    fn without_a_driver_or_an_sdk_nothing_is_available() {
        let d = tempfile::tempdir().unwrap();
        let json = probe_root(Some(d.path()), None, true, None).to_json();
        assert!(json.contains(REASON_NO_SDK), "{json}");
        std::fs::create_dir_all(d.path().join("nvafx/lib")).unwrap();
        std::fs::write(d.path().join("nvafx/lib/libnv_audiofx.so.3"), b"").unwrap();
        let json = probe_root(Some(d.path()), None, false, None).to_json();
        assert_eq!(json.matches(REASON_NO_DRIVER).count(), 5, "{json}");
        let json = probe_root(Some(d.path()), None, true, None).to_json();
        assert_eq!(
            json.matches(tree::REASON_MODELS_NOT_INSTALLED).count(),
            2,
            "speaker focus and bandwidth: {json}"
        );
    }

    #[test]
    fn strengths_are_read_from_ints_and_floats_and_clamped() {
        assert_eq!(strength_of(Some(Value::Int(40))), Some(40));
        assert_eq!(strength_of(Some(Value::Int(400))), Some(100));
        assert_eq!(strength_of(Some(Value::Float(33.6))), Some(34));
        assert_eq!(strength_of(Some(Value::Float(f64::NAN))), None);
        assert_eq!(strength_of(Some(Value::Bool(true))), None);
    }

    #[test]
    fn the_audio_entries_are_exported() {
        // SAFETY: a valid host that lives forever.
        let vt = unsafe {
            obc_module_entry(Box::leak(Box::new(obc_module_sdk::abi::obc_host::empty())))
        };
        assert!(!vt.is_null());
        // SAFETY: a static vtable.
        let vt = unsafe { &*vt };
        assert!(vt.audio_instance_load.is_some() && vt.audio_instance_process.is_some());
    }
}
