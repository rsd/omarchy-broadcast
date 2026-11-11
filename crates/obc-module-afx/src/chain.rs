//! A chain of stages as SDK effects: which effects run ([`plan`]), how
//! each is set up and loaded ([`build`]), and a block through them
//! ([`Chain::process`]).
//!
//! * `mic-denoise` is the `denoiser` effect with model version 2;
//!   `mic-dereverb` the `dereverb` effect; `mic-enhance` the low-latency
//!   studio voice; `mic-focus` the `speaker_focus` effect, alone (not the
//!   SDK's chained `speaker_focus48k_denoiser48k`: noise removal stays its
//!   own stage); `mic-bandwidth` the `superres` effect, 16 kHz to 48 kHz
//!   (see below). When a chain has both `mic-dereverb` and `mic-denoise`
//!   (one request of this module), they run as one `dereverb_denoiser`
//!   effect where it is installed, in the place of the first of them; if
//!   it fails to load, as the two effects.
//! * `intensity_ratio` is set after the load: to 1 on `denoiser`, to the
//!   noise removal strength on `dereverb_denoiser` (again between two
//!   blocks whenever the strength changes). It is not set on `dereverb`,
//!   the studio voice or speaker focus, which has no strength.
//! * The strengths of `denoiser` and `dereverb` are a mix of the effect's
//!   output with its input delayed by the effect's measured delay
//!   ([`Mixer`]); the delayed input is kept for every effect, so a change
//!   of strength changes the mix only.
//! * The studio voice's `mic_profile_model` is set before the load, its
//!   `mic_profile_id` after it.
//! * Bandwidth extension takes 16 kHz and gives 48 kHz: its
//!   `input_sample_rate` (16000) and `output_sample_rate` (48000) are set
//!   before the load, and each 48 kHz block is first decimated to 16 kHz
//!   ([`crate::decimate`], whose state is carried across blocks). Its
//!   frames must be 10 ms: 160 samples in (read before and after the
//!   load) and 480 out (read after it); otherwise the load fails, saying
//!   so. Its measured delay is that of the decimator and the effect
//!   together, as the stage's input reaches its output.
//! * Each effect's delay is measured after its load ([`crate::latency`]);
//!   a stage whose delay cannot be measured fails. The chain's delay is
//!   the sum of its effects'.
//! * An effect that fails on a block is replaced by its delayed input, so
//!   the chain's delay stays what it reported; the effect is destroyed
//!   with the chain (at unload), not on the processing thread.
//! * The SDK's voice activity detection is not enabled: the voice
//!   probability of a block is not reported.

use std::collections::VecDeque;
use std::path::Path;
use std::time::Instant;

use obc_module_sdk::{Error, log};

use crate::decimate::Decimator;
use crate::ffi::{self, Effect};
use crate::latency;
use crate::tree::{self, Effect as Kind};
use crate::{BANDWIDTH, DENOISE, DEREVERB, ENHANCE, FOCUS, Profile};

/// The sample rate the module runs at.
pub const RATE: u32 = 48_000;
/// The samples per block.
pub const FRAME: usize = 480;

/// One effect of a planned chain.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectPlan {
    /// The effect.
    pub kind: Kind,
    /// The stages it runs, in request order.
    pub stages: Vec<&'static str>,
    /// The SDK's `intensity_ratio`, set after the load, when the effect
    /// honours it.
    pub intensity: Option<f32>,
    /// The share of the effect's output in the stage's (the rest is the
    /// delay-matched input): 1 at full strength, and always 1 for the
    /// effects whose strength is not a mix ([`EffectPlan::mixes`]).
    pub mix: f32,
    /// The mic profile, for the studio voice.
    pub profile: Option<Profile>,
}

impl EffectPlan {
    /// Whether the effect's strength is a mix with its delayed input.
    pub fn mixes(&self) -> bool {
        matches!(self.kind, Kind::Denoiser | Kind::Dereverb)
    }
}

/// What a request asks of the chain, besides its stages.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Params {
    /// The noise removal strength, 0 to 1.
    pub noise: f32,
    /// The room echo removal strength, 0 to 1.
    pub echo: f32,
    /// The studio voice's mic profile.
    pub profile: Profile,
}

/// The stage id of the request's `id`, if it is one of the module's.
pub fn stage_id(id: &str) -> Option<&'static str> {
    [BANDWIDTH, FOCUS, DENOISE, DEREVERB, ENHANCE]
        .into_iter()
        .find(|s| *s == id)
}

/// The effects that run `stages` (in order, each once): `combined` says
/// whether the combined noise and room echo model is installed.
pub fn plan(stages: &[&str], params: Params, combined: bool) -> Result<Vec<EffectPlan>, Error> {
    let mut ids: Vec<&'static str> = Vec::new();
    for s in stages {
        let id = stage_id(s).ok_or_else(|| Error::invalid(format!("unknown stage {s:?}")))?;
        if ids.contains(&id) {
            return Err(Error::invalid(format!("stage {id} twice in the chain")));
        }
        ids.push(id);
    }
    let both = combined && ids.contains(&DENOISE) && ids.contains(&DEREVERB);
    let mut out: Vec<EffectPlan> = Vec::new();
    for id in ids {
        let one = |kind, mix, intensity, profile| EffectPlan {
            kind,
            stages: vec![id],
            intensity,
            mix,
            profile,
        };
        match id {
            DENOISE | DEREVERB if both => {
                if let Some(p) = out.iter_mut().find(|p| p.kind == Kind::DereverbDenoiser) {
                    p.stages.push(id);
                } else {
                    out.push(one(Kind::DereverbDenoiser, 1.0, Some(params.noise), None));
                }
            }
            DENOISE => out.push(one(Kind::Denoiser, params.noise, Some(1.0), None)),
            DEREVERB => out.push(one(Kind::Dereverb, params.echo, None, None)),
            FOCUS => out.push(one(Kind::SpeakerFocus, 1.0, None, None)),
            BANDWIDTH => out.push(one(Kind::SuperRes, 1.0, None, None)),
            _ => out.push(one(Kind::StudioVoice, 1.0, None, Some(params.profile))),
        }
    }
    Ok(out)
}

/// A fixed delay of a mono signal.
#[derive(Debug, Clone)]
pub struct DelayLine {
    buf: Vec<f32>,
    pos: usize,
}

impl DelayLine {
    /// A line of `samples` of delay, silent at first.
    pub fn new(samples: usize) -> DelayLine {
        DelayLine {
            buf: vec![0.0; samples],
            pos: 0,
        }
    }

    /// The input `samples` ago, for this input.
    pub fn push(&mut self, x: f32) -> f32 {
        if self.buf.is_empty() {
            return x;
        }
        let y = std::mem::replace(&mut self.buf[self.pos], x);
        self.pos = (self.pos + 1) % self.buf.len();
        y
    }

    /// Silence again.
    pub fn clear(&mut self) {
        self.buf.fill(0.0);
        self.pos = 0;
    }
}

/// The mix of an effect's output with its input delayed to match. The
/// delay line is fed with every block, whatever the mix, so the delayed
/// input is there when the mix changes or the effect fails.
#[derive(Debug, Clone)]
pub struct Mixer {
    /// The effect's share the last block ended at.
    current: f32,
    /// The share asked for, reached over the next block.
    wet: f32,
    dry: DelayLine,
}

impl Mixer {
    /// A mix of `wet` for an effect of `delay` samples.
    pub fn new(wet: f32, delay: usize) -> Mixer {
        let wet = wet.clamp(0.0, 1.0);
        Mixer {
            current: wet,
            wet,
            dry: DelayLine::new(delay),
        }
    }

    /// The effect's share asked for, 0 to 1.
    pub fn wet(&self) -> f32 {
        self.wet
    }

    /// Asks for a share of `wet` (0 to 1), reached linearly over the next
    /// block.
    pub fn set(&mut self, wet: f32) {
        if !wet.is_nan() {
            self.wet = wet.clamp(0.0, 1.0);
        }
    }

    /// `output = wet * output + (1 - wet) * input, delayed`.
    pub fn apply(&mut self, input: &[f32], output: &mut [f32]) {
        let (from, to) = (self.current, self.wet);
        let step = (to - from) / output.len().max(1) as f32;
        for (k, (o, i)) in output.iter_mut().zip(input).enumerate() {
            let dry = self.dry.push(*i);
            let w = if from == to {
                to
            } else {
                from + step * (k + 1) as f32
            };
            if w < 1.0 {
                *o = w * *o + (1.0 - w) * dry;
            }
        }
        self.current = to;
    }

    /// `output = input, delayed`: the stage without its effect.
    pub fn bypass(&mut self, input: &[f32], output: &mut [f32]) {
        for (o, i) in output.iter_mut().zip(input) {
            *o = self.dry.push(*i);
        }
    }

    /// Forgets the delayed input.
    pub fn clear(&mut self) {
        self.dry.clear();
        self.current = self.wet;
    }
}

/// One loaded effect.
#[derive(Debug)]
pub struct Running {
    /// What it was planned as.
    pub plan: EffectPlan,
    /// The effect; `None` once it failed (its stages then pass their input
    /// through, delayed by [`Self::latency`]).
    effect: Option<Effect>,
    /// Its measured delay, in samples.
    pub latency: u32,
    /// The mix with the delayed input.
    mixer: Mixer,
    /// The `intensity_ratio` last set.
    intensity: Option<f32>,
    /// How long its last block took, in milliseconds.
    last_ms: Option<f32>,
    /// The decimation in front of an effect that takes 16 kHz.
    narrow: Option<Narrowband>,
}

/// The 48 kHz to 16 kHz decimation in front of an effect that takes
/// 16 kHz, and the 16 kHz block it makes.
#[derive(Debug)]
struct Narrowband {
    decimator: Decimator,
    block: Vec<f32>,
}

impl Narrowband {
    fn new() -> Narrowband {
        Narrowband {
            decimator: Decimator::new(),
            block: Vec::with_capacity(FRAME),
        }
    }
}

/// Runs one 48 kHz block through `effect`, decimated first by `narrow`
/// when there is one.
fn run_effect(
    effect: &mut Effect,
    narrow: &mut Option<Narrowband>,
    input: &[f32],
    output: &mut [f32],
) -> Result<(), ffi::AfxError> {
    match narrow {
        None => effect.run(input, output),
        Some(n) => {
            n.decimator.process(input, &mut n.block);
            effect.run_resampled(&n.block, output)
        }
    }
}

/// The strengths a block runs at, when given (`None`: as they are).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Strengths {
    /// Noise removal's, 0 to 1.
    pub noise: Option<f32>,
    /// Room echo removal's, 0 to 1.
    pub echo: Option<f32>,
}

/// A loaded chain.
#[derive(Debug)]
pub struct Chain {
    /// The effects, in order.
    pub effects: Vec<Running>,
    a: Vec<f32>,
    b: Vec<f32>,
    /// The failures not reported yet: one per stage, one per block.
    pending: VecDeque<Error>,
    /// The effects that failed, destroyed with the chain.
    retired: Vec<Effect>,
}

/// The SDK parameters the module sets.
mod param {
    pub const INPUT_SAMPLE_RATE: &str = "input_sample_rate";
    pub const OUTPUT_SAMPLE_RATE: &str = "output_sample_rate";
    pub const MODEL_PATH: &str = "model_path";
    pub const EFFECT_VERSION: &str = "effect_version";
    pub const INTENSITY_RATIO: &str = "intensity_ratio";
    pub const FRAME: &str = "num_samples_per_input_frame";
    pub const OUTPUT_FRAME: &str = "num_samples_per_output_frame";
    pub const FRAMES: &str = "supported_num_samples_per_frame";
    pub const MIC_PROFILE_MODEL: &str = "mic_profile_model";
    pub const MIC_PROFILE_ID: &str = "mic_profile_id";
}

/// The denoiser's model version the module uses.
pub const DENOISER_VERSION: u32 = 2;

fn stage_error(plan: &EffectPlan, why: impl std::fmt::Display) -> Error {
    Error::stage(
        plan.stages[0],
        format!("{} ({}): {why}", plan.kind.label(), plan.kind.selector()),
    )
}

/// The errors of `plan`'s effect failing for `why`: one per stage it runs,
/// each naming every one of them.
pub fn failures(plan: &EffectPlan, why: impl std::fmt::Display) -> Vec<Error> {
    let message = if plan.stages.len() > 1 {
        format!(
            "{} ({}, for {}): {why}",
            plan.kind.label(),
            plan.kind.selector(),
            plan.stages.join(" and ")
        )
    } else {
        format!("{} ({}): {why}", plan.kind.label(), plan.kind.selector())
    };
    plan.stages
        .iter()
        .map(|s| Error::stage(*s, message.clone()))
        .collect()
}

/// Creates, sets up and loads the effect of `plan` from the SDK `sdk` at
/// `root`, for a GPU of `cc` (`None`: unknown), then measures its delay.
/// GPU work: from a load only.
pub fn build(
    sdk: &'static ffi::Sdk,
    root: &Path,
    cc: Option<(u32, u32)>,
    plan: &EffectPlan,
) -> Result<Running, Error> {
    let kind = plan.kind;
    let fail = |why: String| stage_error(plan, why);
    let models = tree::models_for_gpu(root, kind, cc).map_err(fail)?;
    let model = tree::main_model(&models, kind)
        .ok_or_else(|| fail(format!("no {} model in {}", kind.model(), models.display())))?;
    let mut effect = sdk
        .create(kind.selector(), &tree::library_dir(root, kind))
        .map_err(fail)?;
    let e = |x: ffi::AfxError| fail(x.to_string());
    let (in_rate, out_rate) = (kind.input_rate(), kind.output_rate());
    // 10 ms blocks at the effect's input rate.
    let in_frame = FRAME * in_rate as usize / RATE as usize;
    let resamples = in_rate != out_rate;
    effect
        .set_u32(param::INPUT_SAMPLE_RATE, in_rate)
        .map_err(e)?;
    if resamples {
        effect
            .set_u32(param::OUTPUT_SAMPLE_RATE, out_rate)
            .map_err(e)?;
    }
    // The block size must be one the effect takes; it is read before the
    // load (the SDK refuses it after).
    match effect.get_u32_list(param::FRAMES) {
        Ok(sizes) if !sizes.contains(&(in_frame as u32)) => {
            return Err(fail(format!(
                "the effect takes blocks of {sizes:?} samples, not {in_frame}{}",
                rate_note(resamples, in_rate)
            )));
        }
        Ok(sizes) if sizes.len() > 1 => {
            if let Err(x) = effect.set_u32(param::FRAME, in_frame as u32) {
                log::debug!("{}: {x}; keeping the default block size", kind.selector());
            }
        }
        Ok(_) => {}
        Err(x) => log::debug!("{}: {x}", kind.selector()),
    }
    if kind == Kind::Denoiser {
        effect
            .set_u32(param::EFFECT_VERSION, DENOISER_VERSION)
            .map_err(e)?;
    }
    effect
        .set_str(param::MODEL_PATH, &model.to_string_lossy())
        .map_err(e)?;
    if kind == Kind::StudioVoice {
        let profile = tree::model_file(&models, tree::MIC_PROFILE_MODEL).ok_or_else(|| {
            fail(format!(
                "no {} model in {}",
                tree::MIC_PROFILE_MODEL,
                models.display()
            ))
        })?;
        effect
            .set_str(param::MIC_PROFILE_MODEL, &profile.to_string_lossy())
            .map_err(e)?;
    }
    effect.load().map_err(|x| {
        fail(format!(
            "{x}. Is the worker started with the SDK's environment and its os-release shim \
             (the drop-in {})?",
            ffi::DROP_IN
        ))
    })?;
    let frame = effect.get_u32(param::FRAME).map_err(e)?;
    if frame as usize != in_frame {
        return Err(fail(format!(
            "the effect takes blocks of {frame} samples, not {in_frame}{}",
            rate_note(resamples, in_rate)
        )));
    }
    if resamples {
        // The output frame size must be known: it is what the run's
        // output buffer is checked against.
        let n = effect.get_u32(param::OUTPUT_FRAME).map_err(|x| {
            fail(format!(
                "the effect's output block size is unknown ({x}), so it cannot be run safely"
            ))
        })?;
        if n as usize != FRAME {
            return Err(fail(format!(
                "the effect gives blocks of {n} samples, not {FRAME} (10 ms at {out_rate} Hz)"
            )));
        }
        effect.set_frames(in_frame, n as usize);
    }
    let mut running = Running {
        plan: plan.clone(),
        effect: Some(effect),
        latency: 0,
        mixer: Mixer::new(1.0, 0),
        intensity: None,
        last_ms: None,
        narrow: resamples.then(Narrowband::new),
    };
    running.apply(plan, true)?;
    Ok(running)
}

/// What a block size refers to, for an effect not at the chain's rate.
fn rate_note(resamples: bool, rate: u32) -> String {
    if resamples {
        format!(" (10 ms at {rate} Hz)")
    } else {
        String::new()
    }
}

impl Running {
    /// Sets what `plan` asks that can change on the loaded effect (its
    /// intensity, its mic profile, the mix), measuring the delay again
    /// when `measure` or the profile changed. A delay that cannot be
    /// measured fails the stage.
    pub fn apply(&mut self, plan: &EffectPlan, measure: bool) -> Result<(), Error> {
        let fail = |why: String| stage_error(plan, why);
        let Some(effect) = self.effect.as_mut() else {
            return Err(fail("the effect failed earlier".into()));
        };
        if let Some(r) = plan.intensity.map(|r| r.clamp(0.0, 1.0))
            && (measure || self.intensity != Some(r))
        {
            effect
                .set_f32(param::INTENSITY_RATIO, r)
                .map_err(|x| fail(x.to_string()))?;
            self.intensity = Some(r);
        }
        let mut measure = measure;
        if let Some(p) = plan.profile
            && (measure || self.plan.profile != Some(p))
        {
            // The SDK's parameter is unsigned; passthrough is -1.
            effect
                .set_u32(param::MIC_PROFILE_ID, p.sdk_id() as u32)
                .map_err(|x| fail(x.to_string()))?;
            measure = true;
        }
        if measure {
            let t = Instant::now();
            // From a clean state (a switch measures a running effect), and
            // left clean for the first block.
            effect.reset().map_err(|x| fail(x.to_string()))?;
            let narrow = &mut self.narrow;
            if let Some(n) = narrow.as_mut() {
                n.decimator.reset();
            }
            let lag = latency::measure(FRAME, |i, o| run_effect(effect, narrow, i, o))
                .map_err(|x| fail(x.to_string()))?;
            effect.reset().map_err(|x| fail(x.to_string()))?;
            if let Some(n) = narrow.as_mut() {
                n.decimator.reset();
            }
            let lag =
                lag.ok_or_else(|| fail("its delay could not be measured after the load".into()))?;
            log::debug!(
                "{}: {lag} samples of delay (measured in {:.0} ms)",
                plan.kind.selector(),
                t.elapsed().as_secs_f32() * 1e3
            );
            self.latency = lag;
            self.mixer = Mixer::new(plan.mix, lag as usize);
        } else {
            self.mixer.set(plan.mix);
        }
        self.plan = plan.clone();
        Ok(())
    }

    /// Whether its effect has not failed.
    pub fn is_intact(&self) -> bool {
        self.effect.is_some()
    }

    /// The strengths of `s` that concern this effect, for the next block.
    fn set_strengths(&mut self, s: Strengths) -> Result<(), ffi::AfxError> {
        match self.plan.kind {
            Kind::Denoiser => s.noise.into_iter().for_each(|w| self.mixer.set(w)),
            Kind::Dereverb => s.echo.into_iter().for_each(|w| self.mixer.set(w)),
            Kind::DereverbDenoiser => {
                if let (Some(r), Some(effect)) = (s.noise, self.effect.as_mut()) {
                    let r = r.clamp(0.0, 1.0);
                    if self.intensity != Some(r) {
                        effect.set_f32(param::INTENSITY_RATIO, r)?;
                        self.intensity = Some(r);
                    }
                }
            }
            Kind::StudioVoice | Kind::SpeakerFocus | Kind::SuperRes => {}
        }
        Ok(())
    }
}

impl Chain {
    /// A chain of `effects`.
    pub fn new(effects: Vec<Running>) -> Chain {
        Chain {
            effects,
            a: vec![0.0; FRAME],
            b: vec![0.0; FRAME],
            pending: VecDeque::new(),
            retired: Vec::new(),
        }
    }

    /// The chain's delay: the sum of its effects' (failed ones included:
    /// they delay their input as much).
    pub fn latency(&self) -> u32 {
        self.effects.iter().map(|r| r.latency).sum()
    }

    /// The stages its intact effects run, in order.
    pub fn stages(&self) -> Vec<&'static str> {
        self.effects
            .iter()
            .filter(|r| r.is_intact())
            .flat_map(|r| r.plan.stages.iter().copied())
            .collect()
    }

    /// The kinds of its effects, in order.
    pub fn kinds(&self) -> Vec<Kind> {
        self.effects.iter().map(|r| r.plan.kind).collect()
    }

    /// Whether no effect has failed.
    pub fn is_intact(&self) -> bool {
        self.effects.iter().all(Running::is_intact)
    }

    /// The failure of effect `i` for `why`: its stages pass their input
    /// through, delayed as the effect did, and the effect waits for the
    /// unload to be destroyed.
    fn fail(&mut self, i: usize, why: impl std::fmt::Display) {
        let r = &mut self.effects[i];
        let errors = failures(&r.plan, why);
        for e in &errors {
            log::warn!("{e}");
        }
        self.pending.extend(errors);
        r.last_ms = None;
        if let Some(effect) = r.effect.take() {
            self.retired.push(effect);
        }
    }

    /// Forgets every effect's state. An effect that fails to reset fails
    /// (see [`Self::take_failures`]).
    pub fn reset(&mut self) {
        for i in 0..self.effects.len() {
            let r = &mut self.effects[i];
            r.mixer.clear();
            if let Some(n) = r.narrow.as_mut() {
                n.decimator.reset();
            }
            if let Some(effect) = r.effect.as_mut()
                && let Err(x) = effect.reset()
            {
                self.fail(i, x);
            }
        }
    }

    /// The failures not reported yet, taken.
    pub fn take_failures(&mut self) -> Vec<Error> {
        self.pending.drain(..).collect()
    }

    /// The step times of the last block: `(first stage of each effect,
    /// ms)`.
    pub fn timings(&self) -> Vec<(String, f32)> {
        self.effects
            .iter()
            .filter_map(|r| r.last_ms.map(|ms| (r.plan.stages[0].to_string(), ms)))
            .collect()
    }

    /// Runs one block from `input` into `output` at `strengths`. An effect
    /// that fails is replaced by its delayed input (its stages fail open,
    /// and the chain's delay stays the same); each of its stages is then
    /// reported, one per block, by an error that names it (the output is
    /// written all the same).
    pub fn process(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        strengths: Strengths,
    ) -> Result<(), Error> {
        self.a.copy_from_slice(input);
        for i in 0..self.effects.len() {
            let r = &mut self.effects[i];
            let t = Instant::now();
            let res = match r.effect.is_some() {
                true => r.set_strengths(strengths).and_then(|()| {
                    let effect = r.effect.as_mut().expect("checked");
                    run_effect(effect, &mut r.narrow, &self.a, &mut self.b)
                }),
                false => Ok(()),
            };
            let r = &mut self.effects[i];
            match (r.effect.is_some(), res) {
                (true, Ok(())) => {
                    r.mixer.apply(&self.a, &mut self.b);
                    r.last_ms = Some(t.elapsed().as_secs_f32() * 1e3);
                }
                (true, Err(x)) => {
                    self.fail(i, x);
                    self.effects[i].mixer.bypass(&self.a, &mut self.b);
                }
                (false, _) => r.mixer.bypass(&self.a, &mut self.b),
            }
            std::mem::swap(&mut self.a, &mut self.b);
        }
        output.copy_from_slice(&self.a);
        match self.pending.pop_front() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> Params {
        Params {
            noise: 0.6,
            echo: 0.3,
            profile: Profile::Warm,
        }
    }

    #[test]
    fn each_stage_is_its_effect_and_the_strengths_go_where_they_are_honoured() {
        let p = plan(&[DENOISE], params(), true).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(
            (p[0].kind, p[0].mix, p[0].intensity),
            (Kind::Denoiser, 0.6, Some(1.0))
        );
        let p = plan(&[DEREVERB], params(), true).unwrap();
        assert_eq!(
            (p[0].kind, p[0].mix, p[0].intensity),
            (Kind::Dereverb, 0.3, None)
        );
        let p = plan(&[ENHANCE], params(), true).unwrap();
        assert_eq!(
            (p[0].kind, p[0].mix, p[0].profile),
            (Kind::StudioVoice, 1.0, Some(Profile::Warm))
        );
        let p = plan(&[FOCUS], params(), true).unwrap();
        assert_eq!(
            (p[0].kind, p[0].mix, p[0].intensity, p[0].profile),
            (Kind::SpeakerFocus, 1.0, None, None)
        );
        assert!(!p[0].mixes());
        let p = plan(&[BANDWIDTH], params(), true).unwrap();
        assert_eq!(
            (p[0].kind, p[0].mix, p[0].intensity, p[0].profile),
            (Kind::SuperRes, 1.0, None, None)
        );
        assert!(!p[0].mixes());
    }

    #[test]
    fn bandwidth_runs_after_noise_and_echo_removal_and_alone() {
        let p = plan(&[FOCUS, DEREVERB, DENOISE, BANDWIDTH], params(), true).unwrap();
        let kinds: Vec<Kind> = p.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            [Kind::SpeakerFocus, Kind::DereverbDenoiser, Kind::SuperRes]
        );
        assert_eq!(p[1].stages, [DEREVERB, DENOISE]);
        assert_eq!(p[2].stages, [BANDWIDTH]);
        // Without the combined model: the two effects, then bandwidth.
        let p = plan(&[DEREVERB, DENOISE, BANDWIDTH], params(), false).unwrap();
        let kinds: Vec<Kind> = p.iter().map(|e| e.kind).collect();
        assert_eq!(kinds, [Kind::Dereverb, Kind::Denoiser, Kind::SuperRes]);
    }

    #[test]
    fn speaker_focus_runs_first_and_alone_before_the_combined_model() {
        let p = plan(&[FOCUS, DEREVERB, DENOISE, ENHANCE], params(), true).unwrap();
        let kinds: Vec<Kind> = p.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            [
                Kind::SpeakerFocus,
                Kind::DereverbDenoiser,
                Kind::StudioVoice
            ]
        );
        assert_eq!(p[0].stages, [FOCUS]);
    }

    #[test]
    fn noise_and_echo_together_are_the_combined_model_where_installed() {
        let p = plan(&[DEREVERB, DENOISE, ENHANCE], params(), true).unwrap();
        let kinds: Vec<Kind> = p.iter().map(|e| e.kind).collect();
        assert_eq!(kinds, [Kind::DereverbDenoiser, Kind::StudioVoice]);
        assert_eq!(p[0].stages, [DEREVERB, DENOISE]);
        assert_eq!((p[0].mix, p[0].intensity), (1.0, Some(0.6)));
        let p = plan(&[DEREVERB, DENOISE], params(), false).unwrap();
        let kinds: Vec<Kind> = p.iter().map(|e| e.kind).collect();
        assert_eq!(kinds, [Kind::Dereverb, Kind::Denoiser]);
    }

    #[test]
    fn unknown_and_repeated_stages_are_refused() {
        assert!(plan(&["denoise"], params(), true).is_err());
        assert!(plan(&[DENOISE, DENOISE], params(), true).is_err());
    }

    #[test]
    fn the_mix_is_delay_matched() {
        let mut m = Mixer::new(0.25, 3);
        let input = [1.0, 0.0, 0.0, 0.0, 0.0];
        // The effect's output: the input three samples late, halved.
        let mut out = [0.0, 0.0, 0.0, 0.5, 0.0];
        m.apply(&input, &mut out);
        assert_eq!(out, [0.0, 0.0, 0.0, 0.25 * 0.5 + 0.75, 0.0]);
        let mut d = DelayLine::new(0);
        assert_eq!(d.push(2.0), 2.0);
    }

    #[test]
    fn a_change_of_mix_ramps_over_one_block_on_a_delay_line_that_was_always_fed() {
        // At full strength the delayed input is kept all the same.
        let mut m = Mixer::new(1.0, 2);
        let mut out = [5.0, 6.0, 7.0, 8.0];
        m.apply(&[1.0, 2.0, 3.0, 4.0], &mut out);
        assert_eq!(out, [5.0, 6.0, 7.0, 8.0], "untouched at full strength");
        m.set(0.0);
        assert_eq!(m.wet(), 0.0);
        let mut out = [0.0; 4];
        m.apply(&[0.0; 4], &mut out);
        // From 1 to 0 over the block: the dry share of 3 and 4 (the last
        // block's) rises; never a step to silence.
        assert_eq!(out, [0.25 * 3.0, 0.5 * 4.0, 0.0, 0.0]);
        let mut out = [9.0; 2];
        m.bypass(&[1.0, 2.0], &mut out);
        assert_eq!(out, [0.0, 0.0]);
        m.set(f32::NAN);
        assert_eq!(m.wet(), 0.0, "a NaN is ignored");
        m.set(3.0);
        assert_eq!(m.wet(), 1.0, "clamped");
    }

    #[test]
    fn a_failed_combined_effect_names_both_stages() {
        let p = plan(&[DEREVERB, DENOISE], params(), true).unwrap();
        let e = failures(&p[0], "Run returned 1 (failed)");
        let stages: Vec<Option<&str>> = e.iter().map(|e| e.failed_stage()).collect();
        assert_eq!(stages, [Some(DEREVERB), Some(DENOISE)]);
        assert!(
            e[0].message().contains("mic-dereverb and mic-denoise"),
            "{e:?}"
        );
        let p = plan(&[DENOISE], params(), true).unwrap();
        let e = failures(&p[0], "x");
        assert_eq!(e.len(), 1);
        assert!(!e[0].message().contains(" and "), "{e:?}");
    }
}
