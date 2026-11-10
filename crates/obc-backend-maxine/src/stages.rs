//! The stages this backend offers, by [`StageId`], and what each of them
//! needs: the SDK features it is made of ([`stage_features`],
//! [`stage_availability_of`]), whether it consumes the matte
//! ([`needs_mask`]), and how the relight stage reads its generic
//! parameters ([`RelightParams`]).
//!
//! | Stage id | SDK features | Runs |
//! |---|---|---|
//! | `background` | green screen, background blur | the matte and the effect of `blur` and `replace` |
//! | `denoise` | denoise | before the mask |
//! | `relight` | green screen, relighting | after the mask, on the person |
//! | `enhance` | video super resolution | in the ingest ([`crate::vsr`]) |
//!
//! # The relight stage's parameters
//!
//! At load time ([`LoadRequest::params`]), under [`RELIGHT_LIGHT`]: the key
//! light's colour, [`Value::Str`] `warm`, `neutral` or `cool` (ASCII case
//! ignored; default `neutral`). A new light on the loaded stage binds a new
//! lighting environment to it and loads no model.
//!
//! Per frame ([`FrameParams::live`]):
//!
//! * [`RELIGHT_DIRECTION`]: the light's direction around the person, in
//!   degrees ([`Value::Int`], or [`Value::Float`]); 0 is the default
//!   position (default 0). The pan is the value as given, clamped to
//!   [`RelightParams::MIN_PAN_DEG`] to [`RelightParams::MAX_PAN_DEG`].
//! * [`RELIGHT_BRIGHTNESS`]: the relit person's brightness, in percent
//!   ([`Value::Int`], or [`Value::Float`]), 100 unchanged (default 100). The
//!   gain is the value divided by 100, clamped to
//!   [`RelightParams::MIN_GAIN`] to [`RelightParams::MAX_GAIN`].
//!
//! A value of another type is refused by [`check_stage_params`] and read as
//! the default by a frame.

use std::fmt;
use std::str::FromStr;

use obc_backend::{
    Availability, BackendError, EffectKind, FrameParams, LoadRequest, Params, StageId, Value,
};

use crate::Feature;

/// The load-time key of the relight stage's light.
pub const RELIGHT_LIGHT: &str = "light";
/// The live key of the relight stage's direction, in degrees.
pub const RELIGHT_DIRECTION: &str = "direction";
/// The live key of the relight stage's brightness, in percent.
pub const RELIGHT_BRIGHTNESS: &str = "brightness";

/// Every stage this backend offers, in the order of the table of the module
/// documentation.
pub const STAGES: [StageId; 4] = [
    StageId::BACKGROUND,
    StageId::DENOISE,
    StageId::RELIGHT,
    StageId::ENHANCE,
];

/// The SDK features `stage` is made of, or `None` for a stage this backend
/// does not offer. A stage that consumes the matte needs the green screen
/// that makes it.
pub fn stage_features(stage: &str) -> Option<&'static [Feature]> {
    Some(match stage {
        "background" => &[Feature::GreenScreen, Feature::BackgroundBlur],
        "denoise" => &[Feature::Denoise],
        "relight" => &[Feature::GreenScreen, Feature::Relighting],
        "enhance" => &[Feature::VideoSuperResolution],
        _ => return None,
    })
}

/// Whether `stage` consumes the green-screen matte: it then runs after the
/// mask (a post-mask stage), and its request makes the backend compute the
/// mask even under [`EffectKind::None`]; a stage that does not runs before
/// the mask (a pre-mask stage).
pub fn needs_mask(stage: &str) -> bool {
    stage == StageId::RELIGHT
}

/// Whether `stage` can be offered, from the availability of each feature
/// (as [`crate::feature_availability`] reports it): installed when every
/// feature of [`stage_features`] is, else not installed with the reason. A
/// stage made of one feature has that feature's reason; one made of several
/// lists `<feature>: <reason>` for each one missing, joined by `; ` (a
/// feature missing from `features` counts as not installed, "not
/// reported"). A stage this backend does not offer is not installed.
pub fn stage_availability_of(features: &[(Feature, Availability)], stage: &str) -> Availability {
    let Some(needs) = stage_features(stage) else {
        return Availability::NotInstalled(format!("the Maxine backend has no stage {stage}"));
    };
    let of = |f: &Feature| features.iter().find(|(g, _)| g == f).map(|(_, a)| a);
    if let [only] = needs {
        return match of(only) {
            Some(a) => a.clone(),
            None => Availability::NotInstalled(format!("{only}: not reported")),
        };
    }
    let missing: Vec<String> = needs
        .iter()
        .filter_map(|f| match of(f) {
            Some(Availability::Installed) => None,
            Some(Availability::NotInstalled(r)) => Some(format!("{f}: {r}")),
            None => Some(format!("{f}: not reported")),
        })
        .collect();
    if missing.is_empty() {
        Availability::Installed
    } else {
        Availability::NotInstalled(missing.join("; "))
    }
}

/// The availability of every stage of [`STAGES`], in that order, from the
/// availability of each feature ([`stage_availability_of`]). Pure: it reads
/// nothing.
pub fn stages_availability(features: &[(Feature, Availability)]) -> Vec<(StageId, Availability)> {
    STAGES
        .iter()
        .map(|s| (s.clone(), stage_availability_of(features, s.as_str())))
        .collect()
}

/// The light of the relight stage: the colour temperature of its key
/// light.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RelightLight {
    /// A warm key light, about 3200 K (tungsten).
    Warm,
    /// A neutral key light, about 5000 K.
    #[default]
    Neutral,
    /// A cool key light, about 6500 K (daylight).
    Cool,
}

impl RelightLight {
    /// Every light, in declaration order.
    pub const ALL: [RelightLight; 3] = [
        RelightLight::Warm,
        RelightLight::Neutral,
        RelightLight::Cool,
    ];

    /// The lower-case name used in configuration, D-Bus and the CLI.
    pub fn as_str(self) -> &'static str {
        match self {
            RelightLight::Warm => "warm",
            RelightLight::Neutral => "neutral",
            RelightLight::Cool => "cool",
        }
    }

    /// The colour temperature of the key light, in kelvin.
    pub fn kelvin(self) -> f32 {
        match self {
            RelightLight::Warm => 3200.0,
            RelightLight::Neutral => 5000.0,
            RelightLight::Cool => 6500.0,
        }
    }
}

impl fmt::Display for RelightLight {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RelightLight {
    type Err = BackendError;
    /// Parses `warm`, `neutral` or `cool`, ignoring ASCII case.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "warm" => Ok(RelightLight::Warm),
            "neutral" => Ok(RelightLight::Neutral),
            "cool" => Ok(RelightLight::Cool),
            _ => Err(BackendError::InvalidInput(format!(
                "unknown relight light {s:?} (warm|neutral|cool)"
            ))),
        }
    }
}

/// What the relight stage does to a frame: the light, where it comes from
/// and how bright the relit foreground is, as the effect takes them (read
/// from the generic parameters with [`RelightParams::from_params`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RelightParams {
    /// The key light's colour.
    pub light: RelightLight,
    /// The direction of the light, in degrees around the vertical axis:
    /// 0 is the default position, positive values turn it one way and
    /// negative values the other ([`RelightParams::MIN_PAN_DEG`] to
    /// [`RelightParams::MAX_PAN_DEG`]).
    pub pan_deg: f32,
    /// The brightness of the relit foreground, 1.0 unchanged
    /// ([`RelightParams::MIN_GAIN`] to [`RelightParams::MAX_GAIN`]).
    pub gain: f32,
}

impl RelightParams {
    /// The smallest light direction, in degrees.
    pub const MIN_PAN_DEG: f32 = -180.0;
    /// The largest light direction, in degrees.
    pub const MAX_PAN_DEG: f32 = 180.0;
    /// The smallest foreground gain.
    pub const MIN_GAIN: f32 = 0.5;
    /// The largest foreground gain.
    pub const MAX_GAIN: f32 = 2.0;
    /// The default foreground gain.
    pub const DEFAULT_GAIN: f32 = 1.0;

    /// The parameters of the relight stage from its load-time `params`
    /// ([`RELIGHT_LIGHT`]) and its `live` ones ([`RELIGHT_DIRECTION`],
    /// [`RELIGHT_BRIGHTNESS`]), not clamped; a key that is absent has its
    /// default. The reason if a value has the wrong type or names no light.
    pub fn from_params(
        params: Option<&Params>,
        live: Option<&Params>,
    ) -> Result<RelightParams, String> {
        let d = RelightParams::default();
        let light = match params.and_then(|p| p.get(RELIGHT_LIGHT)) {
            None => d.light,
            Some(Value::Str(s)) => s.parse::<RelightLight>().map_err(|e| e.to_string())?,
            Some(v) => return Err(wrong_type(RELIGHT_LIGHT, "a string", v)),
        };
        let number = |key: &str| -> Result<Option<f32>, String> {
            match live.and_then(|p| p.get(key)) {
                None => Ok(None),
                Some(Value::Int(i)) => Ok(Some(*i as f32)),
                Some(Value::Float(f)) => Ok(Some(*f as f32)),
                Some(v) => Err(wrong_type(key, "a number", v)),
            }
        };
        let pan_deg = number(RELIGHT_DIRECTION)?.unwrap_or(d.pan_deg);
        let gain = number(RELIGHT_BRIGHTNESS)?.map_or(d.gain, |pct| pct / 100.0);
        Ok(RelightParams {
            light,
            pan_deg,
            gain,
        })
    }

    /// The parameters with the direction and the gain within their ranges;
    /// a value that is not finite becomes its default.
    pub fn clamped(self) -> RelightParams {
        let finite_or = |v: f32, d: f32| if v.is_finite() { v } else { d };
        RelightParams {
            light: self.light,
            pan_deg: finite_or(self.pan_deg, 0.0).clamp(Self::MIN_PAN_DEG, Self::MAX_PAN_DEG),
            gain: finite_or(self.gain, Self::DEFAULT_GAIN).clamp(Self::MIN_GAIN, Self::MAX_GAIN),
        }
    }

    /// Checks the parameters are finite and within their ranges, with the
    /// reason if not (what a client is told; a backend clamps instead).
    pub fn validate(&self) -> Result<(), String> {
        if !self.pan_deg.is_finite()
            || !(Self::MIN_PAN_DEG..=Self::MAX_PAN_DEG).contains(&self.pan_deg)
        {
            return Err(format!(
                "relight direction {} is outside {} to {} degrees",
                self.pan_deg,
                Self::MIN_PAN_DEG,
                Self::MAX_PAN_DEG
            ));
        }
        if !self.gain.is_finite() || !(Self::MIN_GAIN..=Self::MAX_GAIN).contains(&self.gain) {
            return Err(format!(
                "relight gain {} is outside {} to {}",
                self.gain,
                Self::MIN_GAIN,
                Self::MAX_GAIN
            ));
        }
        Ok(())
    }
}

impl Default for RelightParams {
    fn default() -> Self {
        RelightParams {
            light: RelightLight::default(),
            pan_deg: 0.0,
            gain: Self::DEFAULT_GAIN,
        }
    }
}

fn wrong_type(key: &str, want: &str, got: &Value) -> String {
    format!(
        "the relight parameter {key} must be {want}, not a {} ({got})",
        got.kind()
    )
}

/// The relight direction and gain of a frame, clamped as the effect takes
/// them; a value of the wrong type reads as its default.
pub fn relight_frame(params: &FrameParams) -> (f32, f32) {
    let live = params.stage_params(StageId::RELIGHT.as_str());
    let d = RelightParams::default();
    let number = |key: &str| {
        live.and_then(|p| p.get(key)).and_then(|v| match v {
            Value::Int(i) => Some(*i as f32),
            Value::Float(f) => Some(*f as f32),
            _ => None,
        })
    };
    let c = RelightParams {
        light: d.light,
        pan_deg: number(RELIGHT_DIRECTION).unwrap_or(d.pan_deg),
        gain: number(RELIGHT_BRIGHTNESS).map_or(d.gain, |pct| pct / 100.0),
    }
    .clamped();
    (c.pan_deg, c.gain)
}

/// The light the relight stage of `req` asks for, or the stage's failure if
/// the parameter is malformed.
pub(crate) fn relight_light(req: &LoadRequest) -> Result<RelightLight, BackendError> {
    RelightParams::from_params(req.stage_params(StageId::RELIGHT.as_str()), None)
        .map(|p| p.light)
        .map_err(|message| BackendError::Stage {
            stage: StageId::RELIGHT,
            code: crate::ffi::NVCV_ERR_GENERAL,
            message: format!("the relighting could not be loaded: {message}"),
        })
}

/// Checks the parameters a client sets for `stage`, as
/// [`BackendFactory::check_params`](obc_backend::BackendFactory::check_params):
/// for the relight stage, a known light and a direction and brightness of
/// the right type within their ranges (checked whether or not the stage is
/// on). Other stages take no parameters; unknown keys are accepted.
pub fn check_stage_params(stage: &str, params: &Params, live: &Params) -> Result<(), String> {
    if stage == StageId::RELIGHT {
        RelightParams::from_params(Some(params), Some(live))?.validate()?;
    }
    Ok(())
}

/// Whether `req` makes the chain compute the mask: its effect composites
/// through it, or one of its stages consumes it.
pub(crate) fn request_needs_mask(req: &LoadRequest) -> bool {
    req.effect != EffectKind::None || req.stages.iter().any(|s| needs_mask(s.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use Availability::*;
    use std::collections::BTreeMap;

    fn params(kv: &[(&str, Value)]) -> Params {
        kv.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn denoise_is_a_pre_mask_stage_and_relight_a_post_mask_one() {
        assert!(!needs_mask("denoise"));
        assert!(needs_mask("relight"));
        assert!(!needs_mask("hello"));
        let req = |effect, stages: &[StageId]| LoadRequest {
            effect,
            stages: obc_backend::StageList::new(stages.iter().cloned()).unwrap(),
            width: 1280,
            height: 720,
            colour: Default::default(),
            background: None,
            params: BTreeMap::new(),
        };
        assert!(!request_needs_mask(&req(
            EffectKind::None,
            &[StageId::DENOISE]
        )));
        assert!(request_needs_mask(&req(
            EffectKind::None,
            &[StageId::RELIGHT]
        )));
        assert!(request_needs_mask(&req(EffectKind::Blur, &[])));
    }

    #[test]
    fn a_stage_is_available_when_all_its_features_are() {
        let all_installed: Vec<(Feature, Availability)> =
            Feature::ALL.iter().map(|f| (*f, Installed)).collect();
        for stage in &STAGES {
            assert_eq!(
                stage_availability_of(&all_installed, stage.as_str()),
                Installed,
                "{stage}"
            );
        }
        assert_eq!(stage_features("denoise"), Some(&[Feature::Denoise][..]));
        assert_eq!(
            stage_features("relight"),
            Some(&[Feature::GreenScreen, Feature::Relighting][..])
        );
        assert_eq!(
            stage_features("enhance"),
            Some(&[Feature::VideoSuperResolution][..])
        );
        assert_eq!(stage_features("hello"), None);
        let without_green_screen: Vec<(Feature, Availability)> = all_installed
            .iter()
            .map(|(f, a)| match f {
                Feature::GreenScreen => (*f, NotInstalled("no models".into())),
                _ => (*f, a.clone()),
            })
            .collect();
        assert_eq!(
            stage_availability_of(&without_green_screen, "relight"),
            NotInstalled("green-screen: no models".into())
        );
        assert_eq!(
            stage_availability_of(&without_green_screen, "background"),
            NotInstalled("green-screen: no models".into())
        );
        assert_eq!(
            stage_availability_of(&without_green_screen, "denoise"),
            Installed
        );
        let a = stage_availability_of(&[(Feature::GreenScreen, Installed)], "relight");
        assert_eq!(a, NotInstalled("relighting: not reported".into()));
        let a = stage_availability_of(&[], "relight");
        assert_eq!(
            a.reason(),
            Some("green-screen: not reported; relighting: not reported")
        );
        // A stage of one feature has the feature's own reason.
        let no_denoise = [(Feature::Denoise, NotInstalled("no library".into()))];
        assert_eq!(
            stage_availability_of(&no_denoise, "denoise"),
            NotInstalled("no library".into())
        );
        assert_eq!(
            stage_availability_of(&[], "enhance").reason(),
            Some("video-super-resolution: not reported")
        );
        assert!(
            stage_availability_of(&all_installed, "hello")
                .reason()
                .unwrap()
                .contains("no stage hello")
        );
        let every = stages_availability(&without_green_screen);
        let ids: Vec<&str> = every.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(ids, ["background", "denoise", "relight", "enhance"]);
        assert!(!every[0].1.is_installed() && every[1].1.is_installed());
    }

    #[test]
    fn relight_lights_parse_and_display_lower_case() {
        for (s, l, k) in [
            ("warm", RelightLight::Warm, 3200.0),
            ("neutral", RelightLight::Neutral, 5000.0),
            ("cool", RelightLight::Cool, 6500.0),
        ] {
            assert_eq!(s.parse::<RelightLight>().unwrap(), l);
            assert_eq!(l.to_string(), s);
            assert_eq!(l.kelvin(), k);
        }
        assert_eq!("COOL".parse::<RelightLight>().unwrap(), RelightLight::Cool);
        let e = "pink".parse::<RelightLight>().unwrap_err();
        assert!(e.to_string().contains("warm|neutral|cool"), "{e}");
        assert_eq!(RelightLight::default(), RelightLight::Neutral);
        assert_eq!(RelightLight::ALL.len(), 3);
    }

    #[test]
    fn relight_params_clamp_and_validate() {
        let d = RelightParams::default();
        assert_eq!(
            (d.light, d.pan_deg, d.gain),
            (RelightLight::Neutral, 0.0, 1.0)
        );
        assert_eq!(d.clamped(), d);
        assert!(d.validate().is_ok());
        let p = |pan_deg, gain| RelightParams {
            light: RelightLight::Warm,
            pan_deg,
            gain,
        };
        assert_eq!(p(270.0, 3.0).clamped(), p(180.0, 2.0));
        assert_eq!(p(-400.0, 0.1).clamped(), p(-180.0, 0.5));
        assert_eq!(p(f32::NAN, f32::INFINITY).clamped(), p(0.0, 1.0));
        assert_eq!(p(-45.0, 1.25).clamped(), p(-45.0, 1.25));
        for (pan, gain) in [(180.0, 2.0), (-180.0, 0.5), (12.5, 1.5)] {
            assert!(p(pan, gain).validate().is_ok(), "{pan} {gain}");
        }
        for pan in [180.5, -181.0, f32::NAN] {
            let e = p(pan, 1.0).validate().unwrap_err();
            assert!(e.contains("-180 to 180 degrees"), "{e}");
        }
        for gain in [0.49, 2.01, f32::INFINITY] {
            let e = p(0.0, gain).validate().unwrap_err();
            assert!(e.contains("0.5 to 2"), "{e}");
        }
    }

    fn frame(direction: Value, brightness: Value) -> FrameParams {
        FrameParams {
            blur_strength: 0.25,
            live: BTreeMap::from([(
                StageId::RELIGHT,
                params(&[
                    (RELIGHT_DIRECTION, direction),
                    (RELIGHT_BRIGHTNESS, brightness),
                ]),
            )]),
        }
    }

    #[test]
    fn the_generic_params_give_the_daemons_pan_and_gain_exactly() {
        // The daemon's settings are whole degrees and whole percent; the
        // effect takes `degrees as f32` and `percent as f32 / 100.0`.
        for direction in -90i32..=90 {
            for brightness in (50u32..=150).step_by(5) {
                let p = RelightParams::from_params(
                    None,
                    Some(&params(&[
                        (RELIGHT_DIRECTION, direction.into()),
                        (RELIGHT_BRIGHTNESS, brightness.into()),
                    ])),
                )
                .unwrap();
                assert_eq!(p.pan_deg.to_bits(), (direction as f32).to_bits());
                assert_eq!(p.gain.to_bits(), (brightness as f32 / 100.0).to_bits());
                let (pan, gain) = relight_frame(&frame(direction.into(), brightness.into()));
                assert_eq!((pan, gain), (p.pan_deg, p.gain), "{direction} {brightness}");
            }
        }
    }

    #[test]
    fn relight_params_read_the_generic_keys_with_defaults() {
        let d = RelightParams::from_params(None, None).unwrap();
        assert_eq!(d, RelightParams::default());
        let p = RelightParams::from_params(
            Some(&params(&[(RELIGHT_LIGHT, "COOL".into())])),
            Some(&params(&[
                (RELIGHT_DIRECTION, Value::Float(-12.5)),
                (RELIGHT_BRIGHTNESS, Value::Float(75.0)),
                ("unknown", Value::Bool(true)),
            ])),
        )
        .unwrap();
        assert_eq!(
            (p.light, p.pan_deg, p.gain),
            (RelightLight::Cool, -12.5, 0.75)
        );
        let e = RelightParams::from_params(Some(&params(&[(RELIGHT_LIGHT, "pink".into())])), None)
            .unwrap_err();
        assert!(e.contains("warm|neutral|cool"), "{e}");
        let e = RelightParams::from_params(Some(&params(&[(RELIGHT_LIGHT, Value::Int(1))])), None)
            .unwrap_err();
        assert!(e.contains("light must be a string"), "{e}");
        let e =
            RelightParams::from_params(None, Some(&params(&[(RELIGHT_DIRECTION, "left".into())])))
                .unwrap_err();
        assert!(e.contains("direction must be a number"), "{e}");
        // A frame clamps, and reads a wrong type as the default.
        assert_eq!(relight_frame(&FrameParams::default()), (0.0, 1.0));
        assert_eq!(
            relight_frame(&frame(Value::Int(-400), Value::Int(300))),
            (-180.0, 2.0)
        );
        assert_eq!(
            relight_frame(&frame(Value::Int(999), Value::Float(f64::NAN))),
            (180.0, 1.0),
            "a backend clamps"
        );
        assert_eq!(
            relight_frame(&frame("x".into(), Value::Bool(true))),
            (0.0, 1.0)
        );
    }

    #[test]
    fn stage_params_are_checked_for_the_client() {
        let ok = |p: &[(&str, Value)], l: &[(&str, Value)]| {
            check_stage_params("relight", &params(p), &params(l))
        };
        assert!(ok(&[], &[]).is_ok());
        assert!(
            ok(
                &[(RELIGHT_LIGHT, "warm".into())],
                &[
                    (RELIGHT_DIRECTION, Value::Int(-180)),
                    (RELIGHT_BRIGHTNESS, Value::Int(200))
                ]
            )
            .is_ok()
        );
        let e = ok(&[], &[(RELIGHT_DIRECTION, Value::Int(200))]).unwrap_err();
        assert!(e.contains("relight direction"), "{e}");
        let e = ok(&[], &[(RELIGHT_BRIGHTNESS, Value::Int(40))]).unwrap_err();
        assert!(e.contains("relight gain"), "{e}");
        assert!(ok(&[(RELIGHT_LIGHT, "pink".into())], &[]).is_err());
        assert!(
            check_stage_params("denoise", &params(&[("x", Value::Int(1))]), &Params::new()).is_ok()
        );
    }

    #[test]
    fn a_malformed_light_fails_the_stage_alone() {
        let req = LoadRequest {
            effect: EffectKind::Blur,
            stages: StageId::RELIGHT.into(),
            width: 1280,
            height: 720,
            colour: Default::default(),
            background: None,
            params: BTreeMap::from([(StageId::RELIGHT, params(&[(RELIGHT_LIGHT, "pink".into())]))]),
        };
        let e = relight_light(&req).unwrap_err();
        assert!(
            matches!(&e, BackendError::Stage { stage, .. } if *stage == StageId::RELIGHT),
            "{e:?}"
        );
        assert!(e.to_string().contains("warm|neutral|cool"), "{e}");
        let neutral = LoadRequest {
            params: BTreeMap::new(),
            ..req
        };
        assert_eq!(relight_light(&neutral).unwrap(), RelightLight::Neutral);
    }
}
