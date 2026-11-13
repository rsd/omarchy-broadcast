//! The effect of the Process stage (`docs/adr/0012-video-effects-and-maxine-backend.md`):
//! the settings, the backend it runs on, and the `obceffect` element that
//! applies it at the head of the output pipeline.
//!
//! An output built with [`OutputConfig::effect`](crate::OutputConfig::effect)
//! set holds one `obceffect` element for its whole life; the effect, the blur
//! strength, the background, the fallback and the stackable stages
//! ([`EffectSettings::stages`]: each a [`StageId`] with its parameters, for
//! example noise removal, `docs/adr/0018-video-noise-removal.md`, and
//! relighting) change on it while it streams
//! ([`Output::set_effect`](crate::Output::set_effect)), without
//! renegotiating caps. `EffectSettings::default()` (`none`, no stage)
//! inserts the element in passthrough, which costs no copy and no CUDA
//! call; `none` with a stage runs the stages alone. An output built
//! without it (`effect: None`, the default) has no element at all and
//! cannot take an effect later; the daemon builds every session output
//! with it, with or without a backend
//! ([`OutputConfig::effect_backend`](crate::OutputConfig::effect_backend)).
//! The backend can be taken away and given back while the output runs
//! ([`Output::revoke_effect_backend`](crate::Output::revoke_effect_backend),
//! [`Output::set_effect_backend`](crate::Output::set_effect_backend)).
//!
//! The element reports the effect's state as [`EffectStatus`], each
//! stage's as an [`EffectStatus`] of its own
//! ([`Output::stage_status`](crate::Output::stage_status),
//! [`EffectStats::stage`]), its counters as [`EffectStats`], and posts
//! every change of any status on the output pipeline's bus as an element
//! message named [`STATUS_MESSAGE`] (fields `effect`, `status`, `reason`,
//! and `<id>` and `<id>-reason` for every stage the settings list, such as
//! `denoise` and `denoise-reason`). A failure of a stage drops the stage and
//! never fails the effect. The output clock marks the first frame
//! of each new source with [`NEW_SOURCE_FLAG`], on which the element resets
//! the backend's temporal state, and its placeholder image with
//! [`PLACEHOLDER_FLAG`], which the element passes through untouched.

use std::fmt;
use std::sync::Arc;

pub use gst_obc::{
    EffectFallback, EffectKind, EffectSettings, EffectStats, EffectStatus, NEW_SOURCE_FLAG,
    PLACEHOLDER_FLAG, STATUS_MESSAGE, StageRequest, StageStats, fallback_unreadable,
    has_new_source_flag, has_placeholder_flag,
};
use obc_backend::BackendFactory;
pub use obc_backend::{Availability, Params, StageId, StageValue, Value};

use crate::pipeline::{PipelineError, names};

/// The inference backend an output's effect element creates its instance
/// with: a factory the caller provides (the daemon's comes from the module
/// that provides the `background` stage). There is no default: on an output
/// without one ([`crate::OutputConfig::effect_backend`]) every effect fails
/// with the no-backend reason.
#[derive(Clone)]
pub struct EffectBackend(pub Arc<dyn BackendFactory>);

impl EffectBackend {
    /// Any factory.
    pub fn new(factory: Arc<dyn BackendFactory>) -> Self {
        EffectBackend(factory)
    }

    /// The factory.
    pub fn factory(&self) -> &Arc<dyn BackendFactory> {
        &self.0
    }
}

impl fmt::Debug for EffectBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("EffectBackend")
            .field(&self.0.name())
            .finish()
    }
}

/// Whether the effect backend of `factory` can run here, with the reason if
/// not. Cheap: what `BackendFactory::probe` does (no model is loaded and no
/// CUDA context created).
pub fn effect_backend_available_with(factory: &dyn BackendFactory) -> Result<(), String> {
    factory.probe().map_err(|e| e.to_string())
}

/// Checks `settings` as the output applies them, before any backend is
/// asked: the rules of [`EffectSettings::validate`], and for `replace` that
/// the background is an absolute path to a readable regular file within
/// the size limit ([`gst_obc::background::check_file`]). Whether the image
/// decodes is found out when the effect loads.
pub fn check_settings(settings: &EffectSettings) -> Result<(), PipelineError> {
    settings.validate().map_err(PipelineError::Effect)?;
    if settings.kind == EffectKind::Replace
        && let Some(path) = &settings.background
    {
        gst_obc::background::check_file(path).map_err(PipelineError::Effect)?;
    }
    Ok(())
}

/// [`check_settings`], and the stages' parameters as `factory` checks them
/// (`BackendFactory::check_params`).
pub fn check_settings_with(
    settings: &EffectSettings,
    factory: &dyn BackendFactory,
) -> Result<(), PipelineError> {
    check_settings(settings)?;
    for s in &settings.stages {
        factory
            .check_params(&s.id, &s.params, &s.live)
            .map_err(PipelineError::Effect)?;
    }
    if !settings.background_params.is_empty() {
        factory
            .check_params(
                &StageId::BACKGROUND,
                &settings.background_params,
                &Params::new(),
            )
            .map_err(PipelineError::Effect)?;
    }
    Ok(())
}

/// The effect element for an output, configured and named
/// [`names::EFFECT`].
pub(crate) fn effect_element(
    settings: &EffectSettings,
    backend: Option<&EffectBackend>,
) -> Result<gst_obc::ObcEffect, PipelineError> {
    match backend {
        Some(b) => check_settings_with(settings, b.factory().as_ref())?,
        None => check_settings(settings)?,
    }
    gst_obc::register()
        .map_err(|e| PipelineError::Effect(format!("cannot register obceffect: {e}")))?;
    let fx = gst_obc::ObcEffect::new(Some(names::EFFECT));
    fx.set_backend_factory(backend.map(|b| Arc::clone(b.factory())));
    fx.apply(settings).map_err(PipelineError::Effect)?;
    Ok(fx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use obc_backend::{BackendError, InferenceBackend};

    struct Unavailable;
    impl BackendFactory for Unavailable {
        fn name(&self) -> &'static str {
            "unavailable"
        }
        fn probe(&self) -> Result<(), BackendError> {
            Err(BackendError::Unavailable("no such backend here".into()))
        }
        fn create(&self) -> Result<Box<dyn InferenceBackend>, BackendError> {
            Err(BackendError::Unavailable("no such backend here".into()))
        }
    }

    #[test]
    fn availability_carries_the_reason() {
        assert_eq!(
            effect_backend_available_with(&Unavailable),
            Err("no such backend here".to_string())
        );
    }

    /// A backend whose parameter check refuses a relighting direction
    /// beyond 90 degrees, as a real backend checks its own ranges.
    struct Strict;
    impl BackendFactory for Strict {
        fn name(&self) -> &'static str {
            "strict"
        }
        fn probe(&self) -> Result<(), BackendError> {
            Ok(())
        }
        fn create(&self) -> Result<Box<dyn InferenceBackend>, BackendError> {
            Err(BackendError::Unavailable("not for frames".into()))
        }
        fn check_params(
            &self,
            stage: &StageId,
            params: &Params,
            live: &Params,
        ) -> Result<(), String> {
            match live.get("direction").or_else(|| params.get("direction")) {
                Some(Value::Int(d)) if d.abs() > 90 => Err(format!(
                    "the {stage} relight direction {d} is outside -90 to 90"
                )),
                _ => Ok(()),
            }
        }
    }

    /// The element `Output::revoke_effect_backend` and
    /// `Output::set_effect_backend` drive: a revoked backend fails the
    /// effect at once with the no-backend reason, and a backend set after
    /// it re-arms the effect (loading again). No GPU: the element is never
    /// started.
    #[test]
    fn a_revoked_effect_backend_fails_at_once_and_a_new_one_rearms() {
        gst::init().unwrap();
        let backend = EffectBackend::new(Arc::new(Strict));
        let fx = effect_element(&EffectSettings::of(EffectKind::Blur), Some(&backend)).unwrap();
        assert_eq!(fx.status(), EffectStatus::Loading);
        fx.revoke_backend();
        assert!(fx.backend_factory().is_none());
        match fx.status() {
            EffectStatus::Failed(r) => assert!(r.contains("no inference backend"), "{r}"),
            s => panic!("not failed: {s}"),
        }
        // Idempotent.
        fx.revoke_backend();
        assert!(matches!(fx.status(), EffectStatus::Failed(_)));
        fx.set_backend_factory(Some(Arc::clone(backend.factory())));
        assert_eq!(fx.status(), EffectStatus::Loading);
    }

    #[test]
    fn settings_are_checked_with_the_file() {
        assert!(check_settings(&EffectSettings::default()).is_ok());
        assert!(check_settings(&EffectSettings::of(EffectKind::Blur)).is_ok());
        let missing = EffectSettings {
            background: Some("/nonexistent/bg.png".into()),
            ..EffectSettings::of(EffectKind::Replace)
        };
        let e = check_settings(&missing).unwrap_err().to_string();
        assert!(e.contains("unreadable"), "{e}");
        // Blur ignores the background file.
        let blur = EffectSettings {
            kind: EffectKind::Blur,
            ..missing
        };
        assert!(check_settings(&blur).is_ok());
        let e = check_settings(&EffectSettings::of(EffectKind::Replace))
            .unwrap_err()
            .to_string();
        assert!(e.contains("needs a background"), "{e}");
    }

    #[test]
    fn denoise_needs_no_file_and_combines_with_every_effect() {
        for kind in [EffectKind::None, EffectKind::Blur] {
            let s = EffectSettings::of(kind).with_stage(StageRequest::on(StageId::DENOISE));
            assert!(check_settings(&s).is_ok(), "{kind}");
        }
    }

    fn relight(light: &str, direction: i64, brightness: i64) -> StageRequest {
        StageRequest::on(StageId::RELIGHT)
            .with_param("light", light)
            .with_live("direction", direction)
            .with_live("brightness", brightness)
    }

    #[test]
    fn relight_needs_no_file_combines_with_every_effect_and_is_validated() {
        for kind in [EffectKind::None, EffectKind::Blur] {
            let mut s = EffectSettings::of(kind).with_stage(relight("warm", 120, 75));
            if kind == EffectKind::Blur {
                s.stages.insert(0, StageRequest::on(StageId::DENOISE));
            }
            assert!(check_settings(&s).is_ok(), "{kind}");
        }
        let bad = EffectSettings::default().with_stage(relight("neutral", 200, 100));
        let e = check_settings_with(&bad, &Strict).unwrap_err().to_string();
        assert!(e.contains("relight direction"), "{e}");
        // The backend's check applies whether or not the stage is on.
        let mut off = bad.clone();
        off.stages[0].enabled = false;
        assert!(check_settings_with(&off, &Strict).is_err());
        // Without a backend, and with another backend, its rules do not apply.
        assert!(check_settings(&bad).is_ok());
        assert!(check_settings_with(&bad, &Unavailable).is_ok());
    }

    #[test]
    fn the_background_parameters_are_checked_with_the_backend() {
        let mut s = EffectSettings::of(EffectKind::Blur);
        s.background_params
            .insert("direction".to_string(), Value::Int(200));
        let e = check_settings_with(&s, &Strict).unwrap_err().to_string();
        assert!(e.contains("background"), "{e}");
        s.background_params
            .insert("direction".to_string(), Value::Int(10));
        assert!(check_settings_with(&s, &Strict).is_ok());
    }

    #[test]
    fn the_element_is_configured_from_the_settings() {
        gst::init().unwrap();
        let s = EffectSettings {
            blur_strength: 0.3,
            ..EffectSettings::of(EffectKind::Blur)
        }
        .with_stage(StageRequest::on(StageId::DENOISE))
        .with_stage(relight("cool", -30, 120));
        let fx = effect_element(&s, Some(&EffectBackend::new(Arc::new(Unavailable)))).unwrap();
        use gst::prelude::*;
        assert_eq!(fx.name(), names::EFFECT);
        assert_eq!(fx.settings(), s);
        assert_eq!(fx.backend_factory().unwrap().name(), "unavailable");
    }
}
