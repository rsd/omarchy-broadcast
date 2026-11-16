//! An example omarchy-broadcast audio module, built with `obc-module-sdk`.
//!
//! It provides one audio stage, `mic-enhance` (slot `shape`, on the CPU),
//! which changes the level of the voice by a gain in decibels and can mix in
//! a 1 kHz test tone, so that its effect is easy to hear and to measure.
//!
//! The module shows the surface an audio module implements:
//!
//! * [`Module::probe`]: the stage needs nothing, so it is always available;
//! * [`Module::settings`]: an `x_example_hello_audio` group (the vendor
//!   form of a group) of the `mic` surface, with an
//!   on/off switch for the stage, a live `gain_db` parameter and the test
//!   tone's switch and level (load-time);
//! * [`Instance`]: only `unload`, as an audio-only module, with
//!   `obc_module_sdk::no_video_stages!()` for the video entry points, which
//!   refuse video;
//! * [`AudioInstance::load_audio`]: reads the load-time parameters and
//!   checks the block format;
//! * [`AudioInstance::process_audio`]: applies the gain and the tone to one
//!   block.
//!
//! It is exported with `export_module!(HelloAudio, audio)`, which fills
//! the vtable's audio entries.

use std::f64::consts::TAU;

use obc_module_sdk::{
    Apply, AudioBuffer, AudioBufferMut, AudioInstance, AudioLoadReport, AudioRequest, Error, Group,
    Instance, Module, Op, Param, ProbeReport, Role, Setting, SettingsDescription, Value, log,
    serde_json,
};

/// The stage id.
pub const STAGE: &str = "mic-enhance";
/// The parameter key of the gain, in decibels (live).
pub const GAIN_KEY: &str = "gain_db";
/// The parameter key of the test tone's switch (load-time).
pub const TONE_KEY: &str = "tone";
/// The parameter key of the test tone's level, in percent of full scale
/// (load-time).
pub const TONE_LEVEL_KEY: &str = "tone_level_pct";
/// The gain range, in decibels.
pub const GAIN_RANGE_DB: (i64, i64) = (-12, 12);
/// The test tone's level when none is given, in percent.
pub const DEFAULT_TONE_LEVEL: i64 = 10;
/// The test tone's frequency, in hertz.
pub const TONE_HZ: f64 = 1000.0;

/// The module object.
pub struct HelloAudio;

impl Module for HelloAudio {
    type Instance = HelloAudioInstance;

    fn init(_config: serde_json::Value) -> Result<Self, Error> {
        log::debug!("hello audio module initialised");
        Ok(HelloAudio)
    }

    fn probe(&self) -> ProbeReport {
        ProbeReport::new().available(STAGE)
    }

    fn settings(&self) -> SettingsDescription {
        SettingsDescription::new().group(
            Group::new("x_example_hello_audio", "Hello voice")
                .surface("mic")
                .setting(
                    Setting::bool("mic.x_example_hello_audio.enabled", "Voice gain", false)
                        .stage(STAGE)
                        .role(Role::Enable)
                        .hint("switch"),
                )
                .setting(
                    Setting::int(
                        "mic.x_example_hello_audio.gain",
                        "Gain",
                        GAIN_RANGE_DB.0,
                        GAIN_RANGE_DB.1,
                        1,
                        0,
                    )
                    .unit("dB")
                    .stage(STAGE)
                    .key(GAIN_KEY)
                    .apply(Apply::Live)
                    .enabled_when("mic.x_example_hello_audio.enabled", Op::Eq, true)
                    .disabled_reason("Turn on the voice gain to change this")
                    .hint("slider"),
                )
                .setting(
                    Setting::bool("mic.x_example_hello_audio.tone", "Test tone", false)
                        .stage(STAGE)
                        .key(TONE_KEY)
                        .apply(Apply::Reload)
                        .enabled_when("mic.x_example_hello_audio.enabled", Op::Eq, true),
                )
                .setting(
                    Setting::int(
                        "mic.x_example_hello_audio.tone_level",
                        "Tone level",
                        0,
                        100,
                        5,
                        DEFAULT_TONE_LEVEL,
                    )
                    .unit("%")
                    .stage(STAGE)
                    .key(TONE_LEVEL_KEY)
                    .apply(Apply::Reload)
                    .enabled_when("mic.x_example_hello_audio.tone", Op::Eq, true)
                    .disabled_reason("Turn on the test tone to change this"),
                ),
        )
    }

    fn create_instance(&self) -> Result<HelloAudioInstance, Error> {
        Ok(HelloAudioInstance::default())
    }
}

/// One `mic-enhance` chain.
#[derive(Debug, Default)]
pub struct HelloAudioInstance {
    /// The linear gain.
    gain: f32,
    /// The tone's peak level, 0.0 to 1.0; 0.0 for no tone.
    tone_level: f32,
    /// The tone's phase step per sample, in radians.
    tone_step: f64,
    /// The tone's phase at the next sample, in radians (0 to 2π).
    phase: f64,
}

/// The linear gain of `db` decibels.
pub fn gain_of_db(db: f64) -> f32 {
    10f64.powf(db / 20.0) as f32
}

/// The gain in decibels in `value`, clamped to [`GAIN_RANGE_DB`].
fn db_of(value: Option<Value<'_>>) -> Option<f64> {
    value
        .and_then(|v| v.as_float())
        .map(|db| db.clamp(GAIN_RANGE_DB.0 as f64, GAIN_RANGE_DB.1 as f64))
}

/// Writes `input * gain` plus a sine of peak `level` and phase step `step`
/// (from `*phase`, which moves on) into `output`.
pub fn shape(input: &[f32], output: &mut [f32], gain: f32, level: f32, step: f64, phase: &mut f64) {
    for (o, i) in output.iter_mut().zip(input) {
        let mut v = i * gain;
        if level > 0.0 {
            v += level * phase.sin() as f32;
            *phase = (*phase + step) % TAU;
        }
        *o = v;
    }
}

/// An audio-only module: the video entry points refuse video.
impl Instance for HelloAudioInstance {
    obc_module_sdk::no_video_stages!();

    fn unload(&mut self) {
        *self = HelloAudioInstance::default();
    }
}

impl AudioInstance for HelloAudioInstance {
    fn load_audio(&mut self, req: &AudioRequest<'_>) -> Result<AudioLoadReport, Error> {
        if let Some(other) = req.stages().iter().find(|s| **s != STAGE) {
            return Err(Error::invalid(format!("unknown stage {other:?}")));
        }
        if req.channels_in() != req.channels_out() {
            return Err(Error::unsupported(
                "the mic-enhance stage does not change the channel count",
            ));
        }
        self.gain = gain_of_db(db_of(req.param(STAGE, GAIN_KEY)).unwrap_or(0.0));
        let tone = req.param(STAGE, TONE_KEY).and_then(|v| v.as_bool()) == Some(true);
        let level = req
            .param(STAGE, TONE_LEVEL_KEY)
            .and_then(|v| v.as_int())
            .unwrap_or(DEFAULT_TONE_LEVEL)
            .clamp(0, 100);
        self.tone_level = if tone { level as f32 / 100.0 } else { 0.0 };
        self.tone_step = TAU * TONE_HZ / f64::from(req.sample_rate());
        self.phase = 0.0;
        Ok(AudioLoadReport::default())
    }

    fn process_audio(
        &mut self,
        input: &AudioBuffer<'_>,
        output: &mut AudioBufferMut<'_>,
        live: &[Param<'_>],
    ) -> Result<(), Error> {
        if let Some(db) = db_of(Param::find(live, STAGE, GAIN_KEY)) {
            self.gain = gain_of_db(db);
        }
        // Every channel takes the same tone, from the same phase.
        let start = self.phase;
        for c in 0..output.channels() {
            let src = input.plane(c).unwrap_or_default();
            let dst = output.plane_mut(c).unwrap_or_default();
            self.phase = start;
            shape(
                src,
                dst,
                self.gain,
                self.tone_level,
                self.tone_step,
                &mut self.phase,
            );
        }
        Ok(())
    }
}

obc_module_sdk::export_module!(HelloAudio, audio);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decibels_map_to_linear_gains() {
        assert_eq!(gain_of_db(0.0), 1.0);
        assert!((gain_of_db(6.0) - 1.995_262).abs() < 1e-6);
        assert!((gain_of_db(-12.0) - 0.251_189).abs() < 1e-6);
        assert_eq!(db_of(Some(Value::Int(40))), Some(12.0), "clamped");
        assert_eq!(db_of(Some(Value::Float(-3.5))), Some(-3.5));
        assert_eq!(db_of(Some(Value::Bool(true))), None);
    }

    #[test]
    fn the_gain_scales_and_the_tone_adds_a_sine() {
        let input = [0.5f32, -0.25, 0.0, 1.0];
        let mut out = [0.0f32; 4];
        let mut phase = 0.0;
        shape(&input, &mut out, 2.0, 0.0, 0.1, &mut phase);
        assert_eq!(out, [1.0, -0.5, 0.0, 2.0]);
        assert_eq!(phase, 0.0, "no tone, no phase");

        // A quarter period a sample: 0, +level, 0, -level.
        let step = TAU / 4.0;
        let silence = [0.0f32; 4];
        shape(&silence, &mut out, 1.0, 0.5, step, &mut phase);
        let want = [0.0, 0.5, 0.0, -0.5];
        for (o, w) in out.iter().zip(want) {
            assert!((o - w).abs() < 1e-6, "{out:?}");
        }
        assert!(
            phase.abs() < 1e-9 || (phase - TAU).abs() < 1e-9,
            "a whole period: {phase}"
        );
    }

    #[test]
    fn the_settings_are_a_mic_group() {
        let d = HelloAudio.settings();
        assert_eq!(d.groups[0].surface.as_deref(), Some("mic"));
        assert!(
            d.groups[0]
                .settings
                .iter()
                .all(|s| s.id.starts_with("mic.x_example_hello_audio."))
        );
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
