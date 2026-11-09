//! The effect of the Process stage (`docs/adr/0012-video-effects-and-maxine-backend.md`):
//! the settings, the backend it runs on, and the `obceffect` element that
//! applies it at the head of the output pipeline.
//!
//! An output built with [`OutputConfig::effect`](crate::OutputConfig::effect)
//! set holds one `obceffect` element for its whole life; the effect, the blur
//! strength, the background, the fallback and the denoise stage
//! ([`EffectSettings::denoise`], `docs/adr/0018-video-noise-removal.md`)
//! change on it while it streams
//! ([`Output::set_effect`](crate::Output::set_effect)), without
//! renegotiating caps. `EffectSettings::default()` (`none`, no denoise)
//! inserts the element in passthrough, which costs no copy and no CUDA
//! call; `none` with denoise runs the denoise stage alone. An output built
//! without it (`effect: None`, the default) has no element at all and
//! cannot take an effect later.
//!
//! The element reports the effect's state as [`EffectStatus`], the denoise
//! stage's as a second [`EffectStatus`]
//! ([`Output::denoise_status`](crate::Output::denoise_status),
//! [`EffectStats::denoise`]), its counters as [`EffectStats`], and posts
//! every change of either status on the output pipeline's bus as an element
//! message named [`STATUS_MESSAGE`] (fields `effect`, `status`, `reason`,
//! `denoise`, `denoise-reason`). A failure of the denoise stage drops the
//! stage and never fails the effect. The output clock marks the first frame
//! of each new source with [`NEW_SOURCE_FLAG`], on which the element resets
//! the backend's temporal state.

use std::fmt;
use std::sync::Arc;

pub use gst_obc::{
    EffectFallback, EffectKind, EffectSettings, EffectStats, EffectStatus, NEW_SOURCE_FLAG,
    STATUS_MESSAGE, has_new_source_flag,
};
use obc_backend::BackendFactory;
use obc_backend_maxine::MaxineFactory;

use crate::pipeline::{PipelineError, names};

/// The inference backend an output's effect element creates its instance
/// with. [`EffectBackend::default`] is the Maxine backend, configured from
/// the environment (`MAXINE_VFX_ROOT`); tests inject another factory.
#[derive(Clone)]
pub struct EffectBackend(pub Arc<dyn BackendFactory>);

impl EffectBackend {
    /// The Maxine backend from the environment.
    pub fn maxine() -> Self {
        EffectBackend(Arc::new(MaxineFactory::from_env()))
    }

    /// Any factory.
    pub fn new(factory: Arc<dyn BackendFactory>) -> Self {
        EffectBackend(factory)
    }

    /// The factory.
    pub fn factory(&self) -> &Arc<dyn BackendFactory> {
        &self.0
    }
}

impl Default for EffectBackend {
    fn default() -> Self {
        EffectBackend::maxine()
    }
}

impl fmt::Debug for EffectBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("EffectBackend")
            .field(&self.0.name())
            .finish()
    }
}

/// Whether the default effect backend can run here, with the reason if not.
/// Cheap: it opens the libraries and checks the model directory, but loads
/// no model and creates no CUDA context (`BackendFactory::probe`).
pub fn effect_backend_available() -> Result<(), String> {
    effect_backend_available_with(EffectBackend::default().factory().as_ref())
}

/// [`effect_backend_available`] for a given factory.
pub fn effect_backend_available_with(factory: &dyn BackendFactory) -> Result<(), String> {
    factory.probe().map_err(|e| e.to_string())
}

/// Checks `settings` as the output applies them: the rules of
/// [`EffectSettings::validate`], and for `replace` that the background is an
/// absolute path to a readable regular file within the size limit
/// ([`gst_obc::background::check_file`]). Whether the image decodes is
/// found out when the effect loads.
pub fn check_settings(settings: &EffectSettings) -> Result<(), PipelineError> {
    settings.validate().map_err(PipelineError::Effect)?;
    if settings.kind == EffectKind::Replace
        && let Some(path) = &settings.background
    {
        gst_obc::background::check_file(path).map_err(PipelineError::Effect)?;
    }
    Ok(())
}

/// The effect element for an output, configured and named
/// [`names::EFFECT`].
pub(crate) fn effect_element(
    settings: &EffectSettings,
    backend: &EffectBackend,
) -> Result<gst_obc::ObcEffect, PipelineError> {
    check_settings(settings)?;
    gst_obc::register()
        .map_err(|e| PipelineError::Effect(format!("cannot register obceffect: {e}")))?;
    let fx = gst_obc::ObcEffect::new(Some(names::EFFECT));
    fx.set_backend_factory(Some(Arc::clone(backend.factory())));
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

    /// Whether `libVideoFX.so` can be found on the library search path,
    /// which the Maxine factory falls back to.
    fn sdk_on_search_path() -> bool {
        std::env::var_os("LD_LIBRARY_PATH")
            .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("libVideoFX.so").exists()))
    }

    #[test]
    fn the_maxine_backend_with_a_root_that_points_nowhere_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let f = MaxineFactory::with_root(Some(dir.path().join("no-sdk")));
        let r = effect_backend_available_with(&f);
        if sdk_on_search_path() {
            // The factory falls back to the search path, where the SDK is.
            assert_eq!(r, Ok(()));
        } else {
            let e = r.unwrap_err();
            assert!(
                e.contains("not found") && e.contains("no-sdk"),
                "the reason names the root: {e}"
            );
        }
    }

    #[test]
    fn the_default_backend_is_maxine() {
        assert_eq!(EffectBackend::default().factory().name(), "maxine");
        assert_eq!(
            format!("{:?}", EffectBackend::default()),
            "EffectBackend(\"maxine\")"
        );
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
            let s = EffectSettings {
                denoise: true,
                ..EffectSettings::of(kind)
            };
            assert!(check_settings(&s).is_ok(), "{kind}");
        }
    }

    #[test]
    fn the_element_is_configured_from_the_settings() {
        gst::init().unwrap();
        let s = EffectSettings {
            blur_strength: 0.3,
            denoise: true,
            ..EffectSettings::of(EffectKind::Blur)
        };
        let fx = effect_element(&s, &EffectBackend::new(Arc::new(Unavailable))).unwrap();
        use gst::prelude::*;
        assert_eq!(fx.name(), names::EFFECT);
        assert_eq!(fx.settings(), s);
        assert_eq!(fx.backend_factory().unwrap().name(), "unavailable");
    }
}
