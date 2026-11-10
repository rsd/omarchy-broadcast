//! What the effect element is asked to do, what it reports, and the
//! per-frame decision between them: plain data, no GStreamer and no GPU.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

pub use obc_backend::{EffectKind, Params, StageId, Value};
use obc_backend::{FrameParams, StageList};

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

/// One stackable stage of [`EffectSettings::stages`]: whether it runs, and
/// its parameters, whose keys and meaning are the backend's
/// (`obc_backend`, "Stages and their parameters").
///
/// A stage hides nothing (`docs/adr/0018-video-noise-removal.md`), so it
/// never changes what the privacy rules show: a failure of the stage alone
/// drops it and leaves the effect running ([`EffectStatus`] of the effect
/// unchanged; the element's status of the stage says why).
#[derive(Debug, Clone, PartialEq)]
pub struct StageRequest {
    /// The stage.
    pub id: StageId,
    /// Whether it runs, with the effect or alone with [`EffectKind::None`].
    pub enabled: bool,
    /// Its load-time parameters: a change of them is a switch on the
    /// loaded chain (a new request), and forgets the stage's failure.
    pub params: Params,
    /// Its live parameters: they apply on the next frame and load nothing.
    pub live: Params,
}

impl StageRequest {
    /// `id`, off, with no parameters (its defaults).
    pub fn new(id: StageId) -> StageRequest {
        StageRequest {
            id,
            enabled: false,
            params: Params::new(),
            live: Params::new(),
        }
    }

    /// `id`, on, with no parameters (its defaults).
    pub fn on(id: StageId) -> StageRequest {
        StageRequest {
            enabled: true,
            ..StageRequest::new(id)
        }
    }

    /// The request with the load-time parameter `key` set to `value`.
    pub fn with_param(mut self, key: &str, value: impl Into<Value>) -> StageRequest {
        self.params.insert(key.to_owned(), value.into());
        self
    }

    /// The request with the live parameter `key` set to `value`.
    pub fn with_live(mut self, key: &str, value: impl Into<Value>) -> StageRequest {
        self.live.insert(key.to_owned(), value.into());
        self
    }
}

/// Everything that selects the effect and its stages: what
/// [`ObcEffect::apply`](crate::ObcEffect::apply) takes.
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
    /// The stackable stages, in the order the backend gets them (see
    /// `obc_backend` for how a backend runs them around the matte), each id
    /// at most once. A stage that is listed but off is reported (off) by the
    /// element's status message; one that is not listed is off too. Its
    /// parameters are kept while it is off.
    pub stages: Vec<StageRequest>,
}

impl Default for EffectSettings {
    fn default() -> Self {
        EffectSettings {
            kind: EffectKind::None,
            blur_strength: Self::DEFAULT_BLUR_STRENGTH,
            background: None,
            fallback: EffectFallback::Black,
            stages: Vec::new(),
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

    /// The settings with `stage` in the list: replacing the entry of its id,
    /// else appended.
    pub fn with_stage(mut self, stage: StageRequest) -> Self {
        match self.stages.iter_mut().find(|s| s.id == stage.id) {
            Some(s) => *s = stage,
            None => self.stages.push(stage),
        }
        self
    }

    /// The entry of `id`, if it is listed.
    pub fn stage(&self, id: &str) -> Option<&StageRequest> {
        self.stages.iter().find(|s| s.id == id)
    }

    /// The entry of `id`, appended off with no parameters if it is not
    /// listed.
    pub fn stage_mut(&mut self, id: &StageId) -> &mut StageRequest {
        let i = match self.stages.iter().position(|s| s.id == *id) {
            Some(i) => i,
            None => {
                self.stages.push(StageRequest::new(id.clone()));
                self.stages.len() - 1
            }
        };
        &mut self.stages[i]
    }

    /// Whether `id` is listed and on.
    pub fn is_enabled(&self, id: &str) -> bool {
        self.stage(id).is_some_and(|s| s.enabled)
    }

    /// Checks the settings without touching the file system: a strength in
    /// 0..=1, an absolute background path, a background for replace, and
    /// each stage listed once. The stages' parameters are the backend's to
    /// check (`BackendFactory::check_params`, which
    /// [`ObcEffect::apply`](crate::ObcEffect::apply) calls when the element
    /// has a factory).
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
        StageList::new(self.stages.iter().map(|s| s.id.clone()))
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Whether `new` differs from these settings in the parameters of each
    /// frame alone (the blur strength, the stages' live parameters): such a
    /// change loads nothing and is never a retry, so a slider moved after a
    /// failure does not reload what failed on every step.
    pub fn changes_frame_params_only(&self, new: &EffectSettings) -> bool {
        let fixed = |s: &EffectSettings| {
            let mut s = s.clone();
            s.blur_strength = 0.0;
            for st in &mut s.stages {
                st.live.clear();
            }
            s
        };
        self != new && fixed(self) == fixed(new)
    }

    /// The parameters of each frame: the blur strength and the live
    /// parameters of every stage that is on.
    pub fn frame_params(&self) -> FrameParams {
        FrameParams {
            blur_strength: self.blur_strength,
            live: self
                .stages
                .iter()
                .filter(|s| s.enabled && !s.live.is_empty())
                .map(|s| (s.id.clone(), s.live.clone()))
                .collect(),
        }
    }

    /// What the backend must have loaded for these settings: the effect,
    /// for replace its image, and the stages that are on with their
    /// load-time parameters, in order. The blur strength, the fallback, the
    /// live parameters and the parameters of a stage that is off change
    /// nothing that is loaded.
    pub(crate) fn target(&self) -> Target {
        Target {
            kind: self.kind,
            background: match self.kind {
                EffectKind::Replace => self.background.clone(),
                _ => None,
            },
            stages: self
                .stages
                .iter()
                .filter(|s| s.enabled)
                .map(|s| (s.id.clone(), s.params.clone()))
                .collect(),
        }
    }
}

/// What has to be loaded for a request. `kind` none with a stage is the
/// stages alone; none without one loads nothing.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Target {
    pub kind: EffectKind,
    pub background: Option<PathBuf>,
    /// The requested stages, in order, with their load-time parameters.
    pub stages: Vec<(StageId, Params)>,
}

impl Target {
    /// Whether `stage` is requested.
    pub fn wants(&self, stage: &str) -> bool {
        self.stages.iter().any(|(s, _)| s == stage)
    }

    /// Whether any stage is requested.
    pub fn has_stages(&self) -> bool {
        !self.stages.is_empty()
    }

    /// The same request without `stage`.
    pub fn without(&self, stage: &str) -> Target {
        let mut t = self.clone();
        t.stages.retain(|(s, _)| s != stage);
        t
    }

    /// What `stage` asks to load: nothing when it is not requested, else its
    /// load-time parameters. Changes of it forget the stage's recorded
    /// failure.
    pub fn stage_request(&self, stage: &str) -> Option<&Params> {
        self.stages.iter().find(|(s, _)| s == stage).map(|(_, p)| p)
    }

    /// The requested stages, in order.
    pub fn stage_list(&self) -> StageList {
        StageList::new(self.stages.iter().map(|(s, _)| s.clone()))
            .expect("the settings list each stage once (validated)")
    }

    /// The load-time parameters of the requested stages that have any.
    pub fn stage_params(&self) -> BTreeMap<StageId, Params> {
        self.stages
            .iter()
            .filter(|(_, p)| !p.is_empty())
            .map(|(s, p)| (s.clone(), p.clone()))
            .collect()
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

/// The counters and timings of one stage, in [`EffectStats::stages`].
#[derive(Debug, Clone, PartialEq)]
pub struct StageStats {
    /// The stage.
    pub id: StageId,
    /// Its state ([`EffectStatus::Off`] unless it is requested).
    pub status: EffectStatus,
    /// Of the processed frames, those the stage ran on.
    pub frames: u64,
    /// GPU time of the stage alone (the backend's
    /// [`StageTimings::step_ms`](obc_backend::StageTimings::step_ms) of the
    /// stage), median and 95th percentile over the last
    /// [`EffectStats::WINDOW`] frames the stage ran on that the backend
    /// timed; `None` if it times none.
    pub ms_p50: Option<f64>,
    pub ms_p95: Option<f64>,
}

impl StageStats {
    /// The figures of `id` before anything ran.
    pub fn new(id: StageId) -> StageStats {
        StageStats {
            id,
            status: EffectStatus::Off,
            frames: 0,
            ms_p50: None,
            ms_p95: None,
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
    /// Every stage the element has been given, in the order it first saw
    /// them, with its state and figures (see [`EffectStats::stage`]).
    pub stages: Vec<StageStats>,
    /// Frames the backend processed.
    pub frames_processed: u64,
    /// Black frames output (warm-up, or a failure with `on-failure=black`).
    pub frames_black: u64,
    /// Frames passed through untouched (`none`, or a failure with
    /// `on-failure=none`), and input frames copied unprocessed into an
    /// output buffer (a frame whose processing failed, with nothing to hide
    /// or with `on-failure=none`).
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

    /// The figures of stage `id`: those the element has, else (a stage it
    /// was never given) off with nothing counted.
    pub fn stage(&self, id: &StageId) -> StageStats {
        self.stages
            .iter()
            .find(|s| s.id == *id)
            .cloned()
            .unwrap_or_else(|| StageStats::new(id.clone()))
    }
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

    fn relight() -> StageRequest {
        StageRequest::on(StageId::RELIGHT)
    }

    #[test]
    fn a_change_of_the_frame_parameters_alone_is_told_apart() {
        let base = EffectSettings::of(EffectKind::Blur)
            .with_stage(StageRequest::new(StageId::DENOISE))
            .with_stage(relight());
        let with = |f: fn(&mut EffectSettings)| {
            let mut s = base.clone();
            f(&mut s);
            s
        };
        for frame_only in [
            with(|s| s.blur_strength = 0.9),
            with(|s| s.stages[1].live = relight().with_live("direction", 45).live),
            with(|s| s.stages[1].live = relight().with_live("brightness", 150).live),
            with(|s| {
                s.stages[1].live = relight()
                    .with_live("direction", -30)
                    .with_live("brightness", 75)
                    .live;
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
            with(|s| s.stages[1].enabled = false),
            with(|s| s.stages[1].params = relight().with_param("light", "warm").params),
            with(|s| s.stages[0].enabled = true),
            with(|s| s.kind = EffectKind::None),
            with(|s| {
                s.stages[1].live = relight().with_live("direction", 10).live;
                s.fallback = EffectFallback::None;
            }),
            with(|s| s.stages.reverse()),
        ] {
            assert!(!base.changes_frame_params_only(&load), "{load:?}");
        }
    }

    #[test]
    fn stages_are_listed_once_and_found_by_id() {
        let s = EffectSettings::default()
            .with_stage(StageRequest::on(StageId::DENOISE))
            .with_stage(relight().with_param("light", "cool"))
            .with_stage(StageRequest::new(StageId::DENOISE));
        assert_eq!(s.stages.len(), 2, "with_stage replaces the entry of its id");
        assert!(!s.is_enabled("denoise") && s.is_enabled("relight"));
        assert!(!s.is_enabled("hello"));
        assert_eq!(
            s.stage("relight").unwrap().params.get("light"),
            Some(&Value::from("cool"))
        );
        assert!(s.validate().is_ok());
        let mut m = s.clone();
        let hello = StageId::new("hello").unwrap();
        m.stage_mut(&hello).enabled = true;
        assert_eq!(m.stages.last().unwrap().id, hello, "appended");
        m.stage_mut(&StageId::DENOISE).enabled = true;
        assert_eq!(m.stages.len(), 3);
        let mut twice = s.clone();
        twice.stages.push(StageRequest::on(StageId::DENOISE));
        let e = twice.validate().unwrap_err();
        assert!(e.contains("denoise is listed twice"), "{e}");
    }

    #[test]
    fn the_frame_parameters_carry_the_live_parameters_of_the_stages_that_are_on() {
        let s = EffectSettings {
            blur_strength: 0.25,
            ..EffectSettings::of(EffectKind::Blur)
        }
        .with_stage(relight().with_live("direction", -45))
        .with_stage(StageRequest::new(StageId::DENOISE).with_live("x", 1));
        let f = s.frame_params();
        assert_eq!(f.blur_strength, 0.25);
        assert_eq!(
            f.stage_params("relight").and_then(|p| p.get("direction")),
            Some(&Value::Int(-45))
        );
        assert_eq!(f.stage_params("denoise"), None, "off: no live parameters");
        assert_eq!(EffectSettings::default().frame_params().live.len(), 0);
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
        let denoised = blur.clone().with_stage(StageRequest::on(StageId::DENOISE));
        assert!(!blur.target().wants("denoise") && denoised.target().wants("denoise"));
        assert_ne!(blur.target(), denoised.target());
        assert!(!EffectSettings::default().is_enabled("denoise"));
        let listed_off = blur.clone().with_stage(StageRequest::new(StageId::DENOISE));
        assert_eq!(listed_off.target(), blur.target(), "a stage that is off");
        let alone = EffectSettings::default().with_stage(StageRequest::on(StageId::DENOISE));
        assert_eq!(alone.target().kind, EffectKind::None);
        assert!(alone.target().wants("denoise"));
        assert!(alone.validate().is_ok());
    }

    #[test]
    fn the_target_carries_the_relight_request_and_its_light_only_when_on() {
        let off = EffectSettings::of(EffectKind::Blur)
            .with_stage(StageRequest::new(StageId::RELIGHT).with_param("light", "warm"));
        // The light of a stage that is off loads nothing.
        assert_eq!(off.target(), EffectSettings::of(EffectKind::Blur).target());
        let mut on = off.clone();
        on.stage_mut(&StageId::RELIGHT).enabled = true;
        let t = on.target();
        assert!(t.wants("relight") && !t.wants("denoise"));
        let warm = relight().with_param("light", "warm").params;
        assert_eq!(t.stage_request("relight"), Some(&warm));
        assert_eq!(t.stage_request("denoise"), None);
        assert_eq!(t.stage_list().as_slice(), &[StageId::RELIGHT]);
        assert_eq!(t.stage_params().get("relight"), Some(&warm));
        // Direction and gain are per frame: no new target.
        let mut turned = on.clone();
        turned.stage_mut(&StageId::RELIGHT).live = relight()
            .with_live("direction", 90)
            .with_live("brightness", 150)
            .live;
        assert_eq!(turned.target(), t);
        let mut cool = on.clone();
        cool.stage_mut(&StageId::RELIGHT).params = relight().with_param("light", "cool").params;
        assert_ne!(cool.target(), t);
        let both = EffectSettings::of(EffectKind::Blur)
            .with_stage(StageRequest::on(StageId::DENOISE))
            .with_stage(relight().with_param("light", "warm"));
        let b = both.target();
        assert_eq!(
            b.stage_list().as_slice(),
            &[StageId::DENOISE, StageId::RELIGHT]
        );
        assert!(b.has_stages());
        assert_eq!(b.without("denoise"), t);
        assert!(!b.without("denoise").without("relight").has_stages());
        assert_eq!(
            t.without("relight"),
            EffectSettings::of(EffectKind::Blur).target()
        );
        // The settings' order is the target's.
        let reversed = EffectSettings::of(EffectKind::Blur)
            .with_stage(relight())
            .with_stage(StageRequest::on(StageId::DENOISE));
        assert_eq!(
            reversed.target().stage_list().as_slice(),
            &[StageId::RELIGHT, StageId::DENOISE]
        );
        let alone = EffectSettings {
            kind: EffectKind::None,
            ..on
        };
        assert!(alone.validate().is_ok());
        assert_eq!(alone.target().kind, EffectKind::None);
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
