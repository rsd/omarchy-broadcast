//! What the effect element is asked to do, what it reports, and the
//! per-frame decision between them: plain data, no GStreamer and no GPU.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

pub use obc_backend::EffectKind;

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

/// Everything that selects the effect: the element's four properties.
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
}

impl Default for EffectSettings {
    fn default() -> Self {
        EffectSettings {
            kind: EffectKind::None,
            blur_strength: Self::DEFAULT_BLUR_STRENGTH,
            background: None,
            fallback: EffectFallback::Black,
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
    /// 0..=1, an absolute background path, and a background for replace.
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
        Ok(())
    }

    /// What the backend must have loaded for these settings: the effect,
    /// and for replace its image. Blur strength and the fallback change
    /// nothing that is loaded.
    pub(crate) fn target(&self) -> Target {
        Target {
            kind: self.kind,
            background: match self.kind {
                EffectKind::Replace => self.background.clone(),
                _ => None,
            },
        }
    }
}

/// What has to be loaded for a request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Target {
    pub kind: EffectKind,
    pub background: Option<PathBuf>,
}

/// The state of the requested effect.
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
    /// Frames the backend processed.
    pub frames_processed: u64,
    /// Black frames output (warm-up, or a failure with `on-failure=black`).
    pub frames_black: u64,
    /// Frames passed through untouched (`none`, or a failure with
    /// `on-failure=none`).
    pub frames_passthrough: u64,
    /// GPU time of the processing of one frame (CUDA events around the
    /// backend call, on the element's stream), median and 95th percentile
    /// over the last [`EffectStats::WINDOW`] processed frames.
    pub gpu_ms_p50: Option<f64>,
    pub gpu_ms_p95: Option<f64>,
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
pub(crate) fn decide(
    kind: EffectKind,
    status: &EffectStatus,
    fallback: EffectFallback,
    loaded: bool,
) -> FrameAction {
    if kind == EffectKind::None {
        return FrameAction::Passthrough;
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
                    assert_eq!(decide(None, &status, fb, loaded), Passthrough);
                    for kind in [Blur, Replace] {
                        let a = decide(kind, &status, fb, loaded);
                        if a == Passthrough {
                            assert_eq!(
                                (&status, fb),
                                (&failed, EffectFallback::None),
                                "{kind} {status} {fb} {loaded}"
                            );
                        }
                    }
                }
            }
        }
        // Warm-up from none: black; a loaded (previous) effect: processed.
        let fb = EffectFallback::Black;
        assert_eq!(decide(Blur, &EffectStatus::Loading, fb, false), Black);
        assert_eq!(decide(Blur, &EffectStatus::Loading, fb, true), Process);
        assert_eq!(decide(Blur, &EffectStatus::Ready, fb, true), Process);
        assert_eq!(decide(Blur, &failed, fb, true), Black);
        assert_eq!(
            decide(Blur, &failed, EffectFallback::None, true),
            Passthrough
        );
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
