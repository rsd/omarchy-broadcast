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
use std::path::PathBuf;
use std::sync::Arc;

pub use gst_obc::{
    EffectFallback, EffectKind, EffectSettings, EffectStats, EffectStatus, NEW_SOURCE_FLAG,
    STATUS_MESSAGE, has_new_source_flag,
};
use obc_backend::BackendFactory;
use obc_backend_maxine::MaxineFactory;

use crate::pipeline::{PipelineError, names};

/// The inference backend an output's effect element creates its instance
/// with. [`EffectBackend::default`] is the Maxine backend with the SDK root
/// in force when it is made: `MAXINE_VFX_ROOT`, else the configured root
/// ([`set_maxine_root`]), else the package's default root
/// ([`obc_backend_maxine::DEFAULT_ROOT`]), else the library search path.
/// Tests inject another factory.
#[derive(Clone)]
pub struct EffectBackend(pub Arc<dyn BackendFactory>);

impl EffectBackend {
    /// The Maxine backend with the SDK root in force now
    /// ([`obc_backend_maxine::effective_root`]).
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

/// Sets the configured Maxine SDK root of this process (the config file's
/// `[effect] maxine_root`; `None`: not configured). `MAXINE_VFX_ROOT`, when
/// set and non-empty, still takes precedence ([`maxine_root_env`]). Outputs
/// built afterwards use it; an output already built keeps the root it was
/// built with. The SDK is taken from where its first library is loaded in
/// the process (the first probe or load of an effect or of the denoise
/// stage, even one that fails), so after that a root that leads elsewhere
/// fails the effect until the process restarts ([`maxine_root_conflict`]).
pub fn set_maxine_root(root: Option<PathBuf>) {
    obc_backend_maxine::set_configured_root(root);
}

/// Why the Maxine SDK root in force now cannot be used by this process: the
/// SDK is already loaded from elsewhere, which takes a restart to change.
/// `None` when no SDK library is loaded yet or the loaded SDK is the one
/// this root leads to.
pub fn maxine_root_conflict() -> Option<String> {
    obc_backend_maxine::root_conflict(obc_backend_maxine::effective_root().as_deref())
}

/// The Maxine SDK root `MAXINE_VFX_ROOT` sets for this process, when it is
/// set and non-empty: it takes precedence over [`set_maxine_root`].
pub fn maxine_root_env() -> Option<PathBuf> {
    obc_backend_maxine::env_root()
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
        let nowhere = dir.path().join("no-sdk");
        let f = MaxineFactory::with_root(Some(nowhere.clone()));
        let r = effect_backend_available_with(&f);
        if sdk_on_search_path() {
            // The factory falls back to the search path, where the SDK is,
            // and the process is pinned to where it really came from: the
            // search path, which any root without SDK libraries leads to
            // as well, not the root that points nowhere.
            assert_eq!(r, Ok(()));
            assert_eq!(obc_backend_maxine::root_conflict(Some(&nowhere)), None);
            let another = dir.path().join("another-nowhere");
            assert_eq!(obc_backend_maxine::root_conflict(Some(&another)), None);
            assert_eq!(obc_backend_maxine::root_conflict(None), None);
            let elsewhere = dir.path().join("elsewhere/lib");
            std::fs::create_dir_all(&elsewhere).unwrap();
            std::fs::write(elsewhere.join("libVideoFX.so"), b"").unwrap();
            let why = obc_backend_maxine::root_conflict(Some(&dir.path().join("elsewhere")))
                .expect("a root with SDK libraries is another SDK");
            assert!(why.contains("the library search path ("), "{why}");
        } else {
            let e = r.unwrap_err();
            assert!(
                e.contains("not found") && e.contains("no-sdk"),
                "the reason names the root: {e}"
            );
        }
    }

    #[test]
    fn the_configured_root_reaches_the_maxine_backend() {
        let env_set = maxine_root_env().is_some();
        set_maxine_root(Some("/configured/sdk/for/pipeline".into()));
        let root = obc_backend_maxine::configured_root();
        let effective = obc_backend_maxine::effective_root();
        let conflict = maxine_root_conflict();
        set_maxine_root(None);
        assert_eq!(root, Some(PathBuf::from("/configured/sdk/for/pipeline")));
        if !env_set {
            assert_eq!(effective, root);
            // The configured root does not exist, so it leads to the library
            // search path: the only place these unit tests may load the SDK
            // from (the "points nowhere" test, when the SDK is on
            // LD_LIBRARY_PATH), so it never conflicts, in any test order.
            assert_eq!(conflict, None);
        }
        assert_eq!(obc_backend_maxine::configured_root(), None);
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
