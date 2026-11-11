//! The two noise removal engines, block by block: DeepFilterNet 3 (libDF
//! with its embedded model, run by tract) and RNNoise (`nnnoiseless`).
//! Both take 48 kHz mono blocks of [`HOP`] samples, and both keep a
//! constant delay ([`Engine::latency_samples`]) whatever the strength.

use df::tract::{DfParams, DfTract, RuntimeParams};
use ndarray::{ArrayView2, ArrayViewMut2};
use nnnoiseless::DenoiseState;

/// The only sample rate of both engines.
pub const RATE: u32 = 48_000;
/// The samples of a block (10 ms): both engines' hop.
pub const HOP: usize = 480;

/// libDF's stage-skip thresholds (local SNR in dB), set so that no frame
/// skips a stage: `(min_db_thresh, max_db_erb_thresh, max_db_df_thresh)`.
/// The library's defaults (-10, 30, 20) skip so many frames that the
/// output is no better than the noisy input with DeepFilterNet 3.
pub const DF_THRESHOLDS: (f32, f32, f32) = (-100.0, 100.0, 100.0);

/// The weakest DeepFilterNet attenuation limit, in dB, at strength 0. Not
/// 0 dB: libDF takes 0 dB as a bypass that returns the input undelayed,
/// which would change the delay.
pub const DF_MIN_ATTEN_DB: f32 = 1.0;
/// The attenuation limit, in dB, that libDF takes as "no limit": strength
/// 100.
pub const DF_NO_LIMIT_DB: f32 = 100.0;

/// The level of the dither added to a quiet block for DeepFilterNet (an
/// alternating ±value, a 24 kHz tone at -60 dBFS). libDF returns zeros for
/// a block whose mean square is under 1e-7 (about -70 dBFS) without
/// running the model or moving its buffers, so the frames in flight would
/// stay there until the sound comes back, late by the length of the quiet:
/// with the dither every block runs, and the delay stays constant.
pub const DF_DITHER: f32 = 1e-3;
/// The mean square under which a block gets the dither (twice libDF's
/// threshold, for margin).
pub const DF_QUIET_MEAN_SQUARE: f32 = 2e-7;

/// libDF's own silence threshold: a block whose mean square is under this
/// gets zeros from it (the output of such a block is gated to zeros here
/// too, see [`DeepFilter`]).
pub const DF_SILENCE_MEAN_SQUARE: f32 = 1e-7;
/// The blocks of the input that decide the gate of an output block: the one
/// it is aligned with (that many blocks back, as the delay) and the one
/// before it, which the 2-hop STFT window overlaps.
const DF_GATE_BLOCKS: usize = 2;
/// The blocks of delay of DeepFilterNet: 1440 samples.
const DF_DELAY_BLOCKS: usize = 3;
/// The input blocks whose silence is kept: the gate's, back from the
/// aligned one, to the current one.
const DF_SILENT_BLOCKS: usize = DF_DELAY_BLOCKS + 2;

/// RNNoise's delay, in samples: one block.
pub const RNNOISE_LATENCY: u32 = HOP as u32;

/// The scale RNNoise works at (16-bit sample values).
const RNNOISE_SCALE: f32 = 32_768.0;

/// Which engine runs the stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum EngineKind {
    /// DeepFilterNet 3: the better one, about 30 ms of delay.
    #[default]
    DeepFilterNet,
    /// RNNoise: lighter, 10 ms of delay.
    RnNoise,
}

impl EngineKind {
    /// Every engine, the default first.
    pub const ALL: [EngineKind; 2] = [EngineKind::DeepFilterNet, EngineKind::RnNoise];

    /// The value of the engine setting.
    pub fn id(self) -> &'static str {
        match self {
            EngineKind::DeepFilterNet => "deepfilternet",
            EngineKind::RnNoise => "rnnoise",
        }
    }

    /// The label shown.
    pub fn label(self) -> &'static str {
        match self {
            EngineKind::DeepFilterNet => "DeepFilterNet",
            EngineKind::RnNoise => "RNNoise",
        }
    }

    /// The engine of setting value `id`.
    pub fn from_id(id: &str) -> Option<EngineKind> {
        EngineKind::ALL.into_iter().find(|k| k.id() == id)
    }
}

/// The DeepFilterNet attenuation limit, in dB, of `strength` percent: 1 dB
/// at 0 rising exponentially (evenly in dB on a log scale) to 100 dB, no
/// limit, at 100: 25 % is about 3 dB, 50 % 10 dB, 75 % 32 dB.
pub fn df_atten_db(strength: u8) -> f32 {
    let s = f32::from(strength.min(100)) / 100.0;
    if strength >= 100 {
        DF_NO_LIMIT_DB
    } else {
        DF_MIN_ATTEN_DB * (DF_NO_LIMIT_DB / DF_MIN_ATTEN_DB).powf(s)
    }
}

/// libDF's model, and a copy of it as loaded (before any block): what a
/// reset goes back to.
struct Models {
    model: DfTract,
    fresh: DfTract,
}

// SAFETY: `DfTract` is not `Send` only because tract keeps values in `Rc`
// (`TValue`) and op states as `Box<dyn OpState>`, which hold plain buffers
// and no thread-bound resource. Every `Rc` of the two models is created by
// `DfTract::new` and `Clone` here, owned by them, and never handed out: all
// the owners of each count are inside one `Models`, so moving the whole of
// it to another thread moves every owner, and one thread at a time uses it
// (`&mut` access only, from the instance's calls). It is never `Sync`.
// `fresh` is a `Clone` of `model`, and the clone shares what libDF holds
// behind an `Rc`, notably the `erb_buf` and `cplx_buf` scratch buffers,
// which libDF writes in place (`Rc` contents are not copied by `clone`).
// That sharing is within one `Models` (so covered by the argument above) and
// does not break a reset: each of these buffers is rewritten in full before
// it is read in every block, so no state of the stream rides in them, and
// the state a reset restores (the STFT and model state) is cloned apart.
unsafe impl Send for Models {}

/// DeepFilterNet 3, one mono stream.
pub struct DeepFilter {
    models: Models,
    strength: u8,
    latency: u32,
    quiet: [f32; HOP],
    /// Whether each of the last [`DF_SILENT_BLOCKS`] input blocks (the
    /// current one last) was under libDF's silence threshold.
    silent: [bool; DF_SILENT_BLOCKS],
}

impl DeepFilter {
    /// Loads the embedded DeepFilterNet 3 model at `strength` percent.
    pub fn new(strength: u8) -> Result<DeepFilter, String> {
        let (min, erb, df) = DF_THRESHOLDS;
        let rp = RuntimeParams::default_with_ch(1)
            .with_thresholds(min, erb, df)
            .with_atten_lim(df_atten_db(strength));
        let model =
            DfTract::new(DfParams::default(), &rp).map_err(|e| format!("DeepFilterNet: {e:#}"))?;
        if model.hop_size != HOP || model.sr != RATE as usize {
            return Err(format!(
                "DeepFilterNet: the model runs {} samples at {} Hz, not {HOP} at {RATE}",
                model.hop_size, model.sr
            ));
        }
        // The STFT's window less a hop, plus the model's lookahead frames.
        let latency = (model.fft_size - model.hop_size + model.lookahead * model.hop_size) as u32;
        let mut quiet = [0.0; HOP];
        for (i, q) in quiet.iter_mut().enumerate() {
            *q = if i % 2 == 0 { DF_DITHER } else { -DF_DITHER };
        }
        Ok(DeepFilter {
            models: Models {
                fresh: model.clone(),
                model,
            },
            strength: strength.min(100),
            latency,
            quiet,
            silent: [true; DF_SILENT_BLOCKS],
        })
    }

    /// The stage-skip thresholds in force.
    pub fn thresholds(&self) -> (f32, f32, f32) {
        (
            self.models.model.min_db_thresh,
            self.models.model.max_db_erb_thresh,
            self.models.model.max_db_df_thresh,
        )
    }

    /// The attenuation limit in force, as libDF holds it (a linear gain;
    /// `None` for no limit).
    pub fn atten_lim(&self) -> Option<f32> {
        self.models.model.atten_lim
    }

    fn set_strength(&mut self, strength: u8) {
        let strength = strength.min(100);
        if strength != self.strength {
            self.strength = strength;
            self.models.model.set_atten_lim(df_atten_db(strength));
        }
    }

    fn reset(&mut self) {
        self.models.model = self.models.fresh.clone();
        self.models.model.set_atten_lim(df_atten_db(self.strength));
        self.silent = [true; DF_SILENT_BLOCKS];
    }

    fn process(&mut self, input: &[f32], output: &mut [f32]) -> Result<(), String> {
        let mean_square = input.iter().map(|x| x * x).sum::<f32>() / HOP as f32;
        // The delay is exactly DF_DELAY_BLOCKS blocks: the output block is
        // the input block of that many blocks ago, so it is built from the
        // aligned block and the one before it. When every one of them was
        // digital silence, the output is the dither's residue only: zeros.
        self.silent.rotate_left(1);
        self.silent[DF_SILENT_BLOCKS - 1] = mean_square < DF_SILENCE_MEAN_SQUARE;
        let mut dithered = [0.0f32; HOP];
        let input = if mean_square < DF_QUIET_MEAN_SQUARE {
            for ((d, x), q) in dithered.iter_mut().zip(input).zip(&self.quiet) {
                *d = x + q;
            }
            &dithered[..]
        } else {
            input
        };
        let noisy = ArrayView2::from_shape((1, HOP), input).map_err(|e| e.to_string())?;
        let enh = ArrayViewMut2::from_shape((1, HOP), &mut *output).map_err(|e| e.to_string())?;
        self.models
            .model
            .process(noisy, enh)
            .map(drop)
            .map_err(|e| format!("DeepFilterNet: {e:#}"))?;
        // `silent[..2]` are the aligned block and the one before it.
        if self.silent[..DF_GATE_BLOCKS].iter().all(|s| *s) {
            output.fill(0.0);
        }
        Ok(())
    }
}

/// RNNoise, one mono stream, with a wet/dry mix for the strength (the dry
/// signal delayed to match).
pub struct RnNoise {
    state: Box<DenoiseState<'static>>,
    /// The share of the denoised signal in force at the end of the last
    /// block, and the one asked for.
    wet: f32,
    target: f32,
    /// The previous input block: the dry signal, one block late as the
    /// denoised one is.
    dry: [f32; HOP],
}

impl RnNoise {
    /// A fresh RNNoise state at `strength` percent.
    pub fn new(strength: u8) -> RnNoise {
        let wet = f32::from(strength.min(100)) / 100.0;
        RnNoise {
            state: DenoiseState::new(),
            wet,
            target: wet,
            dry: [0.0; HOP],
        }
    }

    fn set_strength(&mut self, strength: u8) {
        self.target = f32::from(strength.min(100)) / 100.0;
    }

    fn reset(&mut self) {
        self.state = DenoiseState::new();
        self.dry = [0.0; HOP];
        self.wet = self.target;
    }

    /// Returns RNNoise's voice probability of the block.
    fn process(&mut self, input: &[f32], output: &mut [f32]) -> f32 {
        let mut scaled = [0.0f32; HOP];
        for (s, x) in scaled.iter_mut().zip(input) {
            *s = x * RNNOISE_SCALE;
        }
        let mut denoised = [0.0f32; HOP];
        let vad = self.state.process_frame(&mut denoised, &scaled);
        // The mix moves to the new strength over the block, sample by
        // sample: no step.
        let (from, to) = (self.wet, self.target);
        for (i, ((o, d), dry)) in output.iter_mut().zip(&denoised).zip(&self.dry).enumerate() {
            let w = from + (to - from) * (i + 1) as f32 / HOP as f32;
            *o = w * (d / RNNOISE_SCALE) + (1.0 - w) * dry;
        }
        self.wet = to;
        self.dry.copy_from_slice(input);
        vad
    }
}

/// One engine of the stage.
pub enum Engine {
    /// DeepFilterNet 3.
    DeepFilter(Box<DeepFilter>),
    /// RNNoise.
    RnNoise(Box<RnNoise>),
}

impl Engine {
    /// Loads engine `kind` at `strength` percent.
    pub fn new(kind: EngineKind, strength: u8) -> Result<Engine, String> {
        Ok(match kind {
            EngineKind::DeepFilterNet => Engine::DeepFilter(Box::new(DeepFilter::new(strength)?)),
            EngineKind::RnNoise => Engine::RnNoise(Box::new(RnNoise::new(strength))),
        })
    }

    /// Which engine this is.
    pub fn kind(&self) -> EngineKind {
        match self {
            Engine::DeepFilter(_) => EngineKind::DeepFilterNet,
            Engine::RnNoise(_) => EngineKind::RnNoise,
        }
    }

    /// The delay of the output behind the input, in samples, its lookahead
    /// included: the same at every strength.
    pub fn latency_samples(&self) -> u32 {
        match self {
            Engine::DeepFilter(d) => d.latency,
            Engine::RnNoise(_) => RNNOISE_LATENCY,
        }
    }

    /// Sets the strength, in percent (clamped to 100), from the next block
    /// on.
    pub fn set_strength(&mut self, strength: u8) {
        match self {
            Engine::DeepFilter(d) => d.set_strength(strength),
            Engine::RnNoise(r) => r.set_strength(strength),
        }
    }

    /// Forgets the stream so far: the next block starts as the first did.
    pub fn reset(&mut self) {
        match self {
            Engine::DeepFilter(d) => d.reset(),
            Engine::RnNoise(r) => r.reset(),
        }
    }

    /// Runs one block of [`HOP`] samples from `input` into `output`;
    /// returns how likely the block holds speech, when the engine says.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> Result<Option<f32>, String> {
        if input.len() != HOP || output.len() != HOP {
            return Err(format!(
                "a block of {} samples in, {} out; {HOP} are needed",
                input.len(),
                output.len()
            ));
        }
        // A non-finite sample would poison the models' state for seconds:
        // it is zero to them.
        let mut clean = [0.0f32; HOP];
        for (c, x) in clean.iter_mut().zip(input) {
            *c = if x.is_finite() { *x } else { 0.0 };
        }
        let voice = match self {
            Engine::DeepFilter(d) => d.process(&clean, output).map(|()| None),
            Engine::RnNoise(r) => Ok(Some(r.process(&clean, output))),
        };
        if voice.is_ok() && output.iter().any(|x| !x.is_finite()) {
            // Not from the input: the state is gone bad. Start over.
            self.reset();
            return Err("the engine's output is not finite; it was reset".to_string());
        }
        voice
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_strength_maps_to_an_attenuation_limit_that_never_bypasses() {
        assert_eq!(df_atten_db(0), DF_MIN_ATTEN_DB);
        assert_eq!(df_atten_db(100), DF_NO_LIMIT_DB);
        assert_eq!(df_atten_db(200), DF_NO_LIMIT_DB);
        assert!((df_atten_db(50) - 10.0).abs() < 1e-3);
        let mut last = 0.0;
        for s in 0..=100 {
            let db = df_atten_db(s);
            assert!(db >= DF_MIN_ATTEN_DB && db > last, "{s}: {db}");
            last = db;
        }
    }

    #[test]
    fn engine_ids_round_trip() {
        for k in EngineKind::ALL {
            assert_eq!(EngineKind::from_id(k.id()), Some(k));
        }
        assert_eq!(EngineKind::from_id("speex"), None);
        assert_eq!(EngineKind::default(), EngineKind::DeepFilterNet);
    }
}
