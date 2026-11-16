//! Microphone noise removal on the CPU, as an omarchy-broadcast audio
//! module (`br.com.dias.voice-cpu`, kind `audio-filter`).
//!
//! One stage, `mic-denoise` (slot `clean`, on the CPU, 48 kHz mono blocks
//! of 480 samples), run by one of two engines:
//!
//! * **DeepFilterNet 3** (the default): libDF with its embedded model, run
//!   by tract, with libDF's stage skipping turned off
//!   ([`engine::DF_THRESHOLDS`]). The strength is the attenuation limit
//!   ([`engine::df_atten_db`]), changed live. About 30 ms of delay.
//! * **RNNoise** (`nnnoiseless`): lighter, 10 ms of delay. The strength is a
//!   wet/dry mix with the dry signal delayed to match, and the block's voice
//!   probability is reported.
//!
//! The settings, of the `mic` surface:
//!
//! * group `noise` ("Noise removal"): `mic.noise.enabled`, the stage's
//!   switch, and `mic.noise.strength` (0 to 100 %, live);
//! * group `cpu` ("Noise removal on the CPU"): `mic.cpu.engine`
//!   (`deepfilternet` or `rnnoise`), which reloads the stage.
//!
//! The stage is always available: it needs nothing but the CPU.

use std::time::Instant;

use obc_module_sdk::{
    Apply, AudioBuffer, AudioBufferMut, AudioInstance, AudioLoadReport, AudioRequest, Error, Group,
    Instance, Module, Op, Param, ProbeReport, Role, Setting, SettingsDescription, Value, log,
    serde_json,
};

pub mod engine;

use engine::{Engine, EngineKind, HOP, RATE};

/// The module id.
pub const ID: &str = "br.com.dias.voice-cpu";
/// The stage id.
pub const STAGE: &str = "mic-denoise";
/// The parameter key of the strength, in percent (live).
pub const STRENGTH_KEY: &str = "strength";
/// The parameter key of the engine (load-time).
pub const ENGINE_KEY: &str = "engine";
/// The strength when none is given, in percent.
pub const DEFAULT_STRENGTH: u8 = 100;

/// The setting ids.
pub const SETTING_ENABLED: &str = "mic.noise.enabled";
/// See [`SETTING_ENABLED`].
pub const SETTING_STRENGTH: &str = "mic.noise.strength";
/// See [`SETTING_ENABLED`].
pub const SETTING_ENGINE: &str = "mic.cpu.engine";

/// The module object.
pub struct VoiceCpu;

impl Module for VoiceCpu {
    type Instance = VoiceInstance;

    fn init(_config: serde_json::Value) -> Result<Self, Error> {
        Ok(VoiceCpu)
    }

    fn probe(&self) -> ProbeReport {
        ProbeReport::new().available(STAGE)
    }

    fn settings(&self) -> SettingsDescription {
        let noise_on = "Turn on noise removal to change this";
        SettingsDescription::new()
            .group(
                Group::new("noise", "Noise removal")
                    .surface("mic")
                    .setting(
                        Setting::bool(SETTING_ENABLED, "Noise removal", true)
                            .stage(STAGE)
                            .role(Role::Enable)
                            .hint("switch"),
                    )
                    .setting(
                        Setting::int(
                            SETTING_STRENGTH,
                            "Strength",
                            0,
                            100,
                            1,
                            i64::from(DEFAULT_STRENGTH),
                        )
                        .unit("%")
                        .stage(STAGE)
                        .key(STRENGTH_KEY)
                        .apply(Apply::Live)
                        .enabled_when(SETTING_ENABLED, Op::Eq, true)
                        .disabled_reason(noise_on)
                        .hint("slider"),
                    ),
            )
            .group(
                Group::new("cpu", "Noise removal on the CPU")
                    .surface("mic")
                    .setting(
                        Setting::enumeration(
                            SETTING_ENGINE,
                            "Engine",
                            EngineKind::ALL.map(|k| (k.id(), k.label())),
                            EngineKind::default().id(),
                        )
                        .stage(STAGE)
                        .key(ENGINE_KEY)
                        .apply(Apply::Reload)
                        .enabled_when(SETTING_ENABLED, Op::Eq, true)
                        .disabled_reason(noise_on),
                    ),
            )
    }

    fn create_instance(&self) -> Result<VoiceInstance, Error> {
        Ok(VoiceInstance::default())
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

/// One `mic-denoise` chain.
#[derive(Default)]
pub struct VoiceInstance {
    engine: Option<Engine>,
    strength: Option<u8>,
    /// How long the last block took, in milliseconds.
    last_ms: Option<f32>,
}

impl VoiceInstance {
    fn strength(&self) -> u8 {
        self.strength.unwrap_or(DEFAULT_STRENGTH)
    }
}

/// An audio-only module: the video entry points refuse video.
impl Instance for VoiceInstance {
    obc_module_sdk::no_video_stages!();

    fn reset_temporal(&mut self) -> Result<Vec<String>, Error> {
        if let Some(e) = &mut self.engine {
            e.reset();
        }
        Ok(Vec::new())
    }

    fn unload(&mut self) {
        self.engine = None;
        self.last_ms = None;
    }

    fn timings(&self) -> Vec<(String, f32)> {
        self.last_ms
            .map(|ms| vec![(STAGE.to_string(), ms)])
            .unwrap_or_default()
    }
}

impl AudioInstance for VoiceInstance {
    fn load_audio(&mut self, req: &AudioRequest<'_>) -> Result<AudioLoadReport, Error> {
        let start = Instant::now();
        if let Some(other) = req.stages().iter().find(|s| **s != STAGE) {
            return Err(Error::invalid(format!("unknown stage {other:?}")));
        }
        if !req.has_stage(STAGE) {
            return Err(Error::invalid(format!("no {STAGE} stage in the request")));
        }
        if req.channels_in() != 1 || req.channels_out() != 1 {
            return Err(Error::unsupported(format!(
                "mic-denoise is mono only: {} channels in, {} out",
                req.channels_in(),
                req.channels_out()
            )));
        }
        if req.sample_rate() != RATE {
            return Err(Error::unsupported(format!(
                "mic-denoise runs at {RATE} Hz, not {}",
                req.sample_rate()
            )));
        }
        if req.frame_samples() as usize != HOP {
            return Err(Error::unsupported(format!(
                "mic-denoise takes blocks of {HOP} samples, not {}",
                req.frame_samples()
            )));
        }
        let kind = match req.param(STAGE, ENGINE_KEY) {
            None => EngineKind::default(),
            Some(v) => v.as_str().and_then(EngineKind::from_id).ok_or_else(|| {
                Error::invalid(format!(
                    "{ENGINE_KEY} {v:?} is not deepfilternet or rnnoise"
                ))
            })?,
        };
        let strength =
            strength_of(req.param(STAGE, STRENGTH_KEY)).unwrap_or_else(|| self.strength());
        let reused = match &mut self.engine {
            Some(e) if e.kind() == kind => {
                e.set_strength(strength);
                true
            }
            _ => {
                // Built aside: a failed switch keeps the engine running, and
                // the strength in force with it.
                let e = Engine::new(kind, strength).map_err(Error::failed)?;
                self.engine = Some(e);
                false
            }
        };
        self.strength = Some(strength);
        let engine = self.engine.as_ref().expect("loaded above");
        let load_ms = start.elapsed().as_secs_f32() * 1e3;
        log::info!(
            "mic-denoise: {} at {strength} %, {} samples of delay (loaded in {load_ms:.0} ms{})",
            kind.label(),
            engine.latency_samples(),
            if reused { ", reused" } else { "" }
        );
        Ok(AudioLoadReport::new(load_ms)
            .with_latency_samples(engine.latency_samples())
            .with_reused_model(reused))
    }

    fn process_audio(
        &mut self,
        input: &AudioBuffer<'_>,
        output: &mut AudioBufferMut<'_>,
        live: &[Param<'_>],
    ) -> Result<(), Error> {
        let start = Instant::now();
        let live_strength = strength_of(Param::find(live, STAGE, STRENGTH_KEY));
        let Some(engine) = &mut self.engine else {
            return Err(Error::state("mic-denoise is not loaded"));
        };
        if let Some(s) = live_strength {
            self.strength = Some(s);
            engine.set_strength(s);
        }
        if input.is_new_source() {
            engine.reset();
        }
        let src = input.plane(0).unwrap_or_default();
        let dst = output.plane_mut(0).unwrap_or_default();
        let voice = match engine.process(src, dst) {
            Ok(v) => v,
            Err(e) => {
                // The stage is dropped from the chain (the ABI's rule for
                // a stage error), and the next block runs without it.
                self.engine = None;
                self.last_ms = None;
                return Err(Error::stage(STAGE, e));
            }
        };
        output.set_voice_probability(voice);
        self.last_ms = Some(start.elapsed().as_secs_f32() * 1e3);
        Ok(())
    }
}

obc_module_sdk::export_module!(VoiceCpu, audio);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strengths_are_read_from_ints_and_floats_and_clamped() {
        assert_eq!(strength_of(Some(Value::Int(40))), Some(40));
        assert_eq!(strength_of(Some(Value::Int(400))), Some(100));
        assert_eq!(strength_of(Some(Value::Int(-4))), Some(0));
        assert_eq!(strength_of(Some(Value::Float(33.6))), Some(34));
        assert_eq!(strength_of(Some(Value::Float(f64::NAN))), None);
        assert_eq!(strength_of(Some(Value::Bool(true))), None);
        assert_eq!(strength_of(None), None);
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
