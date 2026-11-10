//! What the effect element is asked to do, what it reports, and the
//! per-frame decision between them: plain data, no GStreamer and no GPU.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

pub use obc_backend::{EffectKind, RelightLight, RelightParams};
use obc_backend::{Stage, StageList};

/// What the element outputs while an effect is requested but failed
/// (property `on-failure`, configuration `effect.on_failure`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum EffectFallback {
    /// Black frames: the background stays hidden (the default; fails
    /// closed).
    #[default]
    Black,
    /// The input, unprocessed: the user opted into showing the raw image.
    None,
}

impl EffectFallback {
    /// The lower-case name used in configuration and properties.
    pub fn as_str(self) -> &'static str {
        match self {
            EffectFallback::Black => "black",
            EffectFallback::None => "none",
        }
    }
}

impl fmt::Display for EffectFallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EffectFallback {
    type Err = String;
    /// Parses `black` or `none`, ignoring ASCII case.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "black" => Ok(EffectFallback::Black),
            "none" => Ok(EffectFallback::None),
            _ => Err(format!("unknown on-failure value {s:?} (black|none)")),
        }
    }
}

/// Parses an effect name (`none`, `blur`, `replace`), with the reason as a
/// plain string.
pub fn parse_effect(s: &str) -> Result<EffectKind, String> {
    s.parse::<EffectKind>().map_err(|e| e.to_string())
}

/// Everything that selects the effect and its stages: the element's
/// writable properties.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectSettings {
    /// The effect (`effect`).
    pub kind: EffectKind,
    /// Blur strength, 0.0 to 1.0 (`blur-strength`); used by blur only.
    pub blur_strength: f32,
    /// The background image of replace (`background`): an absolute path.
    pub background: Option<PathBuf>,
    /// The output after a failure (`on-failure`).
    pub fallback: EffectFallback,
    /// Run the video noise removal stage (`denoise`) before the effect, or
    /// alone with [`EffectKind::None`]
    /// (`docs/adr/0018-video-noise-removal.md`). It hides nothing, so it
    /// never changes what the privacy rules show: a failure of the stage
    /// alone drops it and leaves the effect running ([`EffectStatus`] of the
    /// effect unchanged; the element's denoise status says why).
    pub denoise: bool,
    /// Run the relighting stage (`relight`) on the person, after the
    /// denoise stage, under the effect or alone with [`EffectKind::None`].
    /// Like denoise it hides nothing: a failure of the stage alone drops it
    /// and leaves the effect running (the element's relight status says
    /// why).
    pub relight: bool,
    /// The relighting stage's light (`relight-light`, a new light is a
    /// switch on the loaded chain), direction (`relight-pan`, degrees) and
    /// foreground gain (`relight-gain`); the last two apply on the next
    /// frame. Kept while the stage is off.
    pub relight_params: RelightParams,
}

impl Default for EffectSettings {
    fn default() -> Self {
        EffectSettings {
            kind: EffectKind::None,
            blur_strength: Self::DEFAULT_BLUR_STRENGTH,
            background: None,
            fallback: EffectFallback::Black,
            denoise: false,
            relight: false,
            relight_params: RelightParams::default(),
        }
    }
}

impl EffectSettings {
    /// The default blur strength.
    pub const DEFAULT_BLUR_STRENGTH: f32 = 0.5;

    /// The given effect with every other setting at its default.
    pub fn of(kind: EffectKind) -> Self {
        EffectSettings {
            kind,
            ..EffectSettings::default()
        }
    }

    /// Checks the settings without touching the file system: a strength in
    /// 0..=1, an absolute background path, a background for replace, and
    /// the relighting direction and gain within their ranges (checked
    /// whether or not the stage is on).
    /// Whether the file is there and decodable is checked when the effect
    /// loads ([`crate::background::check_file`] checks it up front).
    pub fn validate(&self) -> Result<(), String> {
        if !self.blur_strength.is_finite() || !(0.0..=1.0).contains(&self.blur_strength) {
            return Err(format!(
                "blur strength {} is outside 0 to 1",
                self.blur_strength
            ));
        }
        if let Some(p) = &self.background
            && !p.is_absolute()
        {
            return Err(format!(
                "the background image path {} is not absolute",
                p.display()
            ));
        }
        if self.kind == EffectKind::Replace && self.background.is_none() {
            return Err("the replace effect needs a background image".into());
        }
        self.relight_params.validate()
    }

    /// What the backend must have loaded for these settings: the effect,
    /// for replace its image, which stages are requested, and the
    /// relighting light when that stage is. Blur strength, the fallback and
    /// the relighting direction and gain change nothing that is loaded.
    /// Whether `new` differs from these settings in the parameters of each
    /// frame alone (the blur strength, the relighting direction and gain):
    /// such a change loads nothing and is never a retry, so a slider moved
    /// after a failure does not reload what failed on every step.
    pub fn changes_frame_params_only(&self, new: &EffectSettings) -> bool {
        let fixed = |s: &EffectSettings| EffectSettings {
            blur_strength: 0.0,
            relight_params: RelightParams {
                pan_deg: 0.0,
                gain: 0.0,
                ..s.relight_params
            },
            ..s.clone()
        };
        self != new && fixed(self) == fixed(new)
    }

    pub(crate) fn target(&self) -> Target {
        Target {
            kind: self.kind,
            background: match self.kind {
                EffectKind::Replace => self.background.clone(),
                _ => None,
            },
            denoise: self.denoise,
            relight: self.relight,
            relight_light: if self.relight {
                self.relight_params.light
            } else {
                RelightLight::default()
            },
        }
    }
}

/// What has to be loaded for a request. `kind` none with a stage is the
/// stages alone; none without one loads nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Target {
    pub kind: EffectKind,
    pub background: Option<PathBuf>,
    pub denoise: bool,
    pub relight: bool,
    /// The relighting light; the default unless `relight`.
    pub relight_light: RelightLight,
}

impl Target {
    /// Whether `stage` is requested.
    pub fn wants(&self, stage: Stage) -> bool {
        match stage {
            Stage::Denoise => self.denoise,
            Stage::Relight => self.relight,
        }
    }

    /// Whether any stage is requested.
    pub fn has_stages(&self) -> bool {
        Stage::ALL.iter().any(|&s| self.wants(s))
    }

    /// The same request without `stage`.
    pub fn without(&self, stage: Stage) -> Target {
        let mut t = self.clone();
        match stage {
            Stage::Denoise => t.denoise = false,
            Stage::Relight => {
                t.relight = false;
                t.relight_light = RelightLight::default();
            }
        }
        t
    }

    /// What `stage` asks to load: nothing when it is not requested, else its
    /// light (relight) or nothing more (denoise). Changes of it forget the
    /// stage's recorded failure.
    pub fn stage_request(&self, stage: Stage) -> Option<Option<RelightLight>> {
        self.wants(stage).then_some(match stage {
            Stage::Denoise => None,
            Stage::Relight => Some(self.relight_light),
        })
    }

    /// The requested stages, in canonical order.
    pub fn stage_list(&self) -> StageList {
        StageList::new(Stage::ALL.into_iter().filter(|&s| self.wants(s)))
            .expect("Stage::ALL is in canonical order, each once")
    }
}

/// The state of the requested effect, and (as the element's separate
/// denoise status) of the requested denoise stage: `Off` when it is not
/// requested, `Loading` until it runs, `Ready` while it runs on every
/// processed frame, `Failed` with the reason once it was dropped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum EffectStatus {
    /// No effect is requested (`none`): frames pass through.
    #[default]
    Off,
    /// An effect is requested and its models (or its background image) are
    /// loading. Until it is ready the element outputs black frames, or the
    /// previous effect when switching between effects.
    Loading,
    /// The requested effect processes every frame.
    Ready,
    /// The requested effect failed; the reason is fit for the daemon's
    /// `Error` property. The element outputs what `on-failure` says.
    Failed(String),
}

impl EffectStatus {
    /// `off`, `loading`, `ready` or `failed`.
    pub fn as_str(&self) -> &'static str {
        match self {
            EffectStatus::Off => "off",
            EffectStatus::Loading => "loading",
            EffectStatus::Ready => "ready",
            EffectStatus::Failed(_) => "failed",
        }
    }

    /// The failure reason, if failed.
    pub fn reason(&self) -> Option<&str> {
        match self {
            EffectStatus::Failed(r) => Some(r),
            _ => None,
        }
    }
}

impl fmt::Display for EffectStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EffectStatus::Failed(r) => write!(f, "failed: {r}"),
            other => f.write_str(other.as_str()),
        }
    }
}

/// A snapshot of the element's counters and timings.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EffectStats {
    /// The requested effect.
    pub effect: EffectKind,
    /// Its state.
    pub status: EffectStatus,
    /// The state of the denoise stage ([`EffectStatus::Off`] unless it is
    /// requested).
    pub denoise: EffectStatus,
    /// The state of the relighting stage, as `denoise`.
    pub relight: EffectStatus,
    /// Frames the backend processed.
    pub frames_processed: u64,
    /// Black frames output (warm-up, or a failure with `on-failure=black`).
    pub frames_black: u64,
    /// Frames passed through untouched (`none`, or a failure with
    /// `on-failure=none`), and input frames copied unprocessed into an
    /// output buffer (a frame whose processing failed, with nothing to hide
    /// or with `on-failure=none`).
    pub frames_passthrough: u64,
    /// Of the processed frames, those the denoise stage ran on.
    pub frames_denoised: u64,
    /// Of the processed frames, those the relighting stage ran on.
    pub frames_relit: u64,
    /// GPU time of the processing of one frame (CUDA events around the
    /// backend call, on the element's stream), median and 95th percentile
    /// over the last [`EffectStats::WINDOW`] processed frames.
    pub gpu_ms_p50: Option<f64>,
    pub gpu_ms_p95: Option<f64>,
    /// GPU time of the denoise stage alone (the backend's
    /// [`StageTimings::denoise_ms`](obc_backend::StageTimings::denoise_ms)),
    /// median and 95th percentile over the last [`EffectStats::WINDOW`]
    /// denoised frames the backend timed; `None` if it times none.
    pub denoise_ms_p50: Option<f64>,
    pub denoise_ms_p95: Option<f64>,
    /// GPU time of the relighting stage alone, as the denoise figures (the
    /// backend's
    /// [`StageTimings::relight_ms`](obc_backend::StageTimings::relight_ms)),
    /// over the last [`EffectStats::WINDOW`] relit frames it timed.
    pub relight_ms_p50: Option<f64>,
    pub relight_ms_p95: Option<f64>,
    /// Wall time of the last successful load, warm-up included
    /// (`LoadReport::load_ms + warmup_ms`), and whether it reused a loaded
    /// model.
    pub last_load_ms: Option<f64>,
    pub last_load_reused: bool,
    /// Successful loads since the element was created.
    pub loads: u64,
    /// Whether the element holds CUDA resources (its stream, buffer pool
    /// and a context reference): false until an effect is first requested
    /// on a running stream, and again after the element stops.
    pub cuda_active: bool,
}

impl EffectStats {
    /// The number of processed frames the GPU percentiles cover.
    pub const WINDOW: usize = 300;
}

/// What one frame gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum FrameAction {
    /// The input buffer downstream, untouched (BaseTransform passthrough).
    #[default]
    Passthrough,
    /// A black frame in a new output buffer.
    Black,
    /// The loaded effect into a new output buffer (black if, by the time
    /// the backend is locked, nothing is loaded any more).
    Process,
}

/// The decision of the privacy rules (ADR-0012): the raw input is shown only
/// for `none`, or after a failure with `on-failure=none`; an effect that is
/// not loaded yet shows black; a loaded effect (the requested one, or the
/// previous one while the requested one loads) processes the frame.
///
/// `loaded` says a chain usable for the request is loaded: for blur and
/// replace one with an effect (the stages alone hide nothing), for `none`
/// any. `stages` says a stage is effectively requested (requested, and not
/// failed). The stages hide nothing, so they change nothing for blur and
/// replace; `none` with one is processed once loaded and passed through
/// until then, and after the stages failed (ADR-0018).
pub(crate) fn decide(
    kind: EffectKind,
    status: &EffectStatus,
    fallback: EffectFallback,
    loaded: bool,
    stages: bool,
) -> FrameAction {
    if kind == EffectKind::None {
        return if stages && loaded {
            FrameAction::Process
        } else {
            FrameAction::Passthrough
        };
    }
    match (status, fallback) {
        (EffectStatus::Failed(_), EffectFallback::Black) => FrameAction::Black,
        (EffectStatus::Failed(_), EffectFallback::None) => FrameAction::Passthrough,
        _ if loaded => FrameAction::Process,
        _ => FrameAction::Black,
    }
}

/// The `p`-th percentile (0 to 100, nearest rank) of `values`.
pub(crate) fn percentile(values: &[f32], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut v: Vec<f32> = values.to_vec();
    v.sort_by(f32::total_cmp);
    let rank = ((p / 100.0) * v.len() as f64).ceil() as usize;
    Some(f64::from(v[rank.clamp(1, v.len()) - 1]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_of_the_frame_parameters_alone_is_told_apart() {
        let base = EffectSettings {
            relight: true,
            ..EffectSettings::of(EffectKind::Blur)
        };
        let with = |f: fn(&mut EffectSettings)| {
            let mut s = base.clone();
            f(&mut s);
            s
        };
        for frame_only in [
            with(|s| s.blur_strength = 0.9),
            with(|s| s.relight_params.pan_deg = 45.0),
            with(|s| s.relight_params.gain = 1.5),
            with(|s| {
                s.relight_params.pan_deg = -30.0;
                s.relight_params.gain = 0.75;
                s.blur_strength = 0.1;
            }),
        ] {
            assert!(
                base.changes_frame_params_only(&frame_only),
                "{frame_only:?}"
            );
        }
        // The same settings again are a retry, not a frame change.
        assert!(!base.changes_frame_params_only(&base.clone()));
        for load in [
            with(|s| s.relight = false),
            with(|s| s.relight_params.light = RelightLight::Warm),
            with(|s| s.denoise = true),
            with(|s| s.kind = EffectKind::None),
            with(|s| {
                s.relight_params.pan_deg = 10.0;
                s.fallback = EffectFallback::None;
            }),
        ] {
            assert!(!base.changes_frame_params_only(&load), "{load:?}");
        }
    }

    #[test]
    fn fallback_parses_and_displays() {
        assert_eq!("black".parse::<EffectFallback>(), Ok(EffectFallback::Black));
        assert_eq!("NONE".parse::<EffectFallback>(), Ok(EffectFallback::None));
        assert!(
            "raw"
                .parse::<EffectFallback>()
                .unwrap_err()
                .contains("black|none")
        );
        assert_eq!(EffectFallback::default().to_string(), "black");
        assert_eq!(parse_effect("Replace"), Ok(EffectKind::Replace));
        assert!(
            parse_effect("sepia")
                .unwrap_err()
                .contains("none|blur|replace")
        );
    }

    #[test]
    fn settings_validation() {
        assert!(EffectSettings::default().validate().is_ok());
        assert!(EffectSettings::of(EffectKind::Blur).validate().is_ok());
        let e = EffectSettings::of(EffectKind::Replace)
            .validate()
            .unwrap_err();
        assert!(e.contains("background"), "{e}");
        let replace = EffectSettings {
            background: Some("/srv/bg.png".into()),
            ..EffectSettings::of(EffectKind::Replace)
        };
        assert!(replace.validate().is_ok());
        let rel = EffectSettings {
            background: Some("bg.png".into()),
            ..replace.clone()
        };
        assert!(rel.validate().unwrap_err().contains("not absolute"));
        for s in [-0.1, 1.01, f32::NAN, f32::INFINITY] {
            let bad = EffectSettings {
                blur_strength: s,
                ..EffectSettings::of(EffectKind::Blur)
            };
            assert!(bad.validate().unwrap_err().contains("0 to 1"), "{s}");
        }
    }

    #[test]
    fn the_target_carries_the_denoise_request() {
        let blur = EffectSettings::of(EffectKind::Blur);
        let denoised = EffectSettings {
            denoise: true,
            ..blur.clone()
        };
        assert!(!blur.target().denoise && denoised.target().denoise);
        assert_ne!(blur.target(), denoised.target());
        assert!(!EffectSettings::default().denoise);
        let alone = EffectSettings {
            denoise: true,
            ..EffectSettings::default()
        };
        assert_eq!(alone.target().kind, EffectKind::None);
        assert!(alone.target().denoise);
        assert!(alone.validate().is_ok());
    }

    #[test]
    fn the_target_carries_the_relight_request_and_its_light_only_when_on() {
        let off = EffectSettings {
            relight_params: RelightParams {
                light: RelightLight::Warm,
                ..RelightParams::default()
            },
            ..EffectSettings::of(EffectKind::Blur)
        };
        // The light of a stage that is off loads nothing.
        assert_eq!(off.target(), EffectSettings::of(EffectKind::Blur).target());
        let on = EffectSettings {
            relight: true,
            ..off.clone()
        };
        let t = on.target();
        assert!(t.relight && t.wants(Stage::Relight) && !t.wants(Stage::Denoise));
        assert_eq!(t.relight_light, RelightLight::Warm);
        assert_eq!(
            t.stage_request(Stage::Relight),
            Some(Some(RelightLight::Warm))
        );
        assert_eq!(t.stage_request(Stage::Denoise), None);
        assert_eq!(t.stage_list().as_slice(), &[Stage::Relight]);
        // Direction and gain are per frame: no new target.
        let turned = EffectSettings {
            relight_params: RelightParams {
                pan_deg: 90.0,
                gain: 1.5,
                ..on.relight_params
            },
            ..on.clone()
        };
        assert_eq!(turned.target(), t);
        let cool = EffectSettings {
            relight_params: RelightParams {
                light: RelightLight::Cool,
                ..on.relight_params
            },
            ..on.clone()
        };
        assert_ne!(cool.target(), t);
        let both = EffectSettings {
            denoise: true,
            ..on.clone()
        };
        let b = both.target();
        assert_eq!(b.stage_list().as_slice(), &[Stage::Denoise, Stage::Relight]);
        assert!(b.has_stages());
        assert_eq!(b.without(Stage::Denoise), t);
        assert!(
            !b.without(Stage::Denoise)
                .without(Stage::Relight)
                .has_stages()
        );
        assert_eq!(
            t.without(Stage::Relight),
            EffectSettings::of(EffectKind::Blur).target()
        );
        let alone = EffectSettings {
            kind: EffectKind::None,
            ..on
        };
        assert!(alone.validate().is_ok());
        assert_eq!(alone.target().kind, EffectKind::None);
    }

    #[test]
    fn relight_direction_and_gain_are_validated() {
        for (pan_deg, gain) in [
            (181.0, 1.0),
            (-180.5, 1.0),
            (0.0, 0.4),
            (0.0, 2.1),
            (f32::NAN, 1.0),
        ] {
            let s = EffectSettings {
                relight_params: RelightParams {
                    pan_deg,
                    gain,
                    ..RelightParams::default()
                },
                ..EffectSettings::default()
            };
            assert!(
                s.validate().unwrap_err().contains("relight"),
                "{pan_deg} {gain}"
            );
        }
        let edge = EffectSettings {
            relight: true,
            relight_params: RelightParams {
                pan_deg: -180.0,
                gain: 2.0,
                ..RelightParams::default()
            },
            ..EffectSettings::default()
        };
        assert!(edge.validate().is_ok());
        assert!(!EffectSettings::default().relight);
    }

    #[test]
    fn the_target_ignores_what_needs_no_reload() {
        let blur = EffectSettings {
            background: Some("/a.png".into()),
            blur_strength: 0.9,
            ..EffectSettings::of(EffectKind::Blur)
        };
        assert_eq!(blur.target(), EffectSettings::of(EffectKind::Blur).target());
        let a = EffectSettings {
            background: Some("/a.png".into()),
            ..EffectSettings::of(EffectKind::Replace)
        };
        let b = EffectSettings {
            background: Some("/b.png".into()),
            ..a.clone()
        };
        assert_ne!(a.target(), b.target());
    }

    #[test]
    fn the_raw_input_is_shown_only_when_asked_for() {
        use EffectKind::*;
        use FrameAction::*;
        let failed = EffectStatus::Failed("x".into());
        for status in [
            EffectStatus::Off,
            EffectStatus::Loading,
            EffectStatus::Ready,
            failed.clone(),
        ] {
            for fb in [EffectFallback::Black, EffectFallback::None] {
                for loaded in [false, true] {
                    // none: always the input.
                    assert_eq!(decide(None, &status, fb, loaded, false), Passthrough);
                    for kind in [Blur, Replace] {
                        let a = decide(kind, &status, fb, loaded, false);
                        if a == Passthrough {
                            assert_eq!(
                                (&status, fb),
                                (&failed, EffectFallback::None),
                                "{kind} {status} {fb} {loaded}"
                            );
                        }
                        // The denoise stage hides nothing: it changes no
                        // decision of an effect.
                        assert_eq!(
                            decide(kind, &status, fb, loaded, true),
                            a,
                            "{kind} {status} {fb} {loaded}"
                        );
                    }
                }
            }
        }
        // Warm-up from none: black; a loaded (previous) effect: processed.
        let fb = EffectFallback::Black;
        assert_eq!(
            decide(Blur, &EffectStatus::Loading, fb, false, false),
            Black
        );
        assert_eq!(
            decide(Blur, &EffectStatus::Loading, fb, true, false),
            Process
        );
        assert_eq!(decide(Blur, &EffectStatus::Ready, fb, true, false), Process);
        assert_eq!(decide(Blur, &failed, fb, true, false), Black);
        assert_eq!(
            decide(Blur, &failed, EffectFallback::None, true, false),
            Passthrough
        );
    }

    #[test]
    fn none_with_denoise_is_processed_once_loaded_and_never_black() {
        use EffectKind::None;
        use FrameAction::*;
        for status in [
            EffectStatus::Off,
            EffectStatus::Loading,
            EffectStatus::Ready,
            EffectStatus::Failed("x".into()),
        ] {
            for fb in [EffectFallback::Black, EffectFallback::None] {
                // Loading (or reloading): the input, never black.
                assert_eq!(decide(None, &status, fb, false, true), Passthrough);
                // Loaded: the denoise stage runs.
                assert_eq!(decide(None, &status, fb, true, true), Process);
                // Not requested, or failed (the effective request is off):
                // the input, with no CUDA.
                assert_eq!(decide(None, &status, fb, true, false), Passthrough);
            }
        }
    }

    #[test]
    fn percentiles_are_nearest_rank() {
        assert_eq!(percentile(&[], 50.0), None);
        let v: Vec<f32> = (1..=100).map(|i| i as f32).collect();
        assert_eq!(percentile(&v, 50.0), Some(50.0));
        assert_eq!(percentile(&v, 95.0), Some(95.0));
        assert_eq!(percentile(&[3.0], 95.0), Some(3.0));
    }

    #[test]
    fn status_text() {
        assert_eq!(EffectStatus::Ready.to_string(), "ready");
        let f = EffectStatus::Failed("no SDK".into());
        assert_eq!(f.to_string(), "failed: no SDK");
        assert_eq!(f.reason(), Some("no SDK"));
        assert_eq!(EffectStatus::default(), EffectStatus::Off);
    }
}
