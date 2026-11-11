//! The SDK tree on disk: where the root is, which effects are installed
//! and where their models are. Reads directories only: nothing here opens
//! a library or touches the GPU, so the probe never wakes a suspended GPU.
//!
//! The layout (that of NVIDIA's download and of the `maxine-afx-sdk-local`
//! package): `<root>/nvafx/lib` holds the core library;
//! `<root>/features/<feature>/lib` an effect's feature library; and
//! `<root>/features/<feature>/models/sm_<NN>/` its models for GPUs of
//! compute capability N.N.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::ffi;

/// The environment variable that names the SDK root, after the
/// configuration's `sdk_root`.
pub const ROOT_ENV: &str = "MAXINE_AFX_ROOT";

/// Where the `maxine-afx-sdk-local` package installs the SDK.
pub const DEFAULT_ROOT: &str = "/opt/nvidia/maxine/afx";

/// A file the NVIDIA kernel driver provides while it is loaded (reading
/// its presence wakes no GPU).
pub const DRIVER_FILE: &str = "/proc/driver/nvidia/version";

/// An effect of the SDK the module uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Effect {
    /// Noise removal (the `denoiser` effect, model version 2).
    Denoiser,
    /// Room echo removal (`dereverb`).
    Dereverb,
    /// Noise and room echo removal in one model (`dereverb_denoiser`).
    DereverbDenoiser,
    /// Studio voice, low latency (`studio_voice_low_latency`).
    StudioVoice,
    /// Speaker focus (`speaker_focus`): keeps the main, nearest speaker
    /// and removes other voices and background talk.
    ///
    /// The SDK's header names this effect only chained with noise removal
    /// (`speaker_focus48k_denoiser48k`). The feature directory
    /// (`speaker_focus`), the standalone selector (`speaker_focus`, so the
    /// feature library `libnv_audiofx_speaker_focus.so`) and the model name
    /// (`speaker_focus_48k`) follow the layout NVIDIA's feature download
    /// uses for the other effects; they are unverified until the feature
    /// is installed.
    SpeakerFocus,
}

impl Effect {
    /// Every effect.
    pub const ALL: [Effect; 5] = [
        Effect::Denoiser,
        Effect::Dereverb,
        Effect::DereverbDenoiser,
        Effect::StudioVoice,
        Effect::SpeakerFocus,
    ];

    /// The SDK's selector of the effect.
    pub fn selector(self) -> &'static str {
        match self {
            Effect::Denoiser => "denoiser",
            Effect::Dereverb => "dereverb",
            Effect::DereverbDenoiser => "dereverb_denoiser",
            Effect::StudioVoice => "studio_voice_low_latency",
            Effect::SpeakerFocus => "speaker_focus",
        }
    }

    /// The directory of its feature under `<root>/features`.
    pub fn feature(self) -> &'static str {
        match self {
            Effect::StudioVoice => "studio_voice",
            other => other.selector(),
        }
    }

    /// The name of its 48 kHz model (the file is `<name>` and
    /// [`MODEL_EXT`], or `<name>_<digits>` and [`MODEL_EXT`]: see
    /// [`model_file`]).
    pub fn model(self) -> &'static str {
        match self {
            Effect::Denoiser => "denoiser_v2_48k",
            Effect::Dereverb => "dereverb_48k",
            Effect::DereverbDenoiser => "dereverb_denoiser_48k",
            Effect::StudioVoice => "studio_voice_low_latency_48k",
            Effect::SpeakerFocus => "speaker_focus_48k",
        }
    }

    /// The other models it needs, in its feature's model directory.
    pub fn extra_models(self) -> &'static [&'static str] {
        match self {
            Effect::StudioVoice => &[MIC_PROFILE_MODEL],
            _ => &[],
        }
    }

    /// What the user calls it, in reasons.
    pub fn label(self) -> &'static str {
        match self {
            Effect::Denoiser => "noise removal",
            Effect::Dereverb => "room echo removal",
            Effect::DereverbDenoiser => "noise and room echo removal",
            Effect::StudioVoice => "studio voice",
            Effect::SpeakerFocus => "speaker focus",
        }
    }

    /// The one reason given whenever its feature, its library or its
    /// models (for this GPU) are missing, when the effect has one; `None`:
    /// the reason names what is missing.
    pub fn not_installed(self) -> Option<&'static str> {
        match self {
            Effect::SpeakerFocus => Some(REASON_MODELS_NOT_INSTALLED),
            _ => None,
        }
    }
}

/// The probe's reason when speaker focus, an Early Access feature that the
/// SDK's download does not install by default, is not installed (its
/// feature, library or models).
pub const REASON_MODELS_NOT_INSTALLED: &str = "Models not installed";

/// The file name extension of a model.
pub const MODEL_EXT: &str = ".trtpkg";

/// The studio voice effect's mic profile model.
pub const MIC_PROFILE_MODEL: &str = "studio_voice_mic_profile_48k";

/// The SDK root: `configured` (the module's `sdk_root`) when not empty,
/// else `env` (the value of [`ROOT_ENV`]) when set and not empty, else
/// `default` when it is a directory, else none.
pub fn resolve_root(
    configured: Option<&Path>,
    env: Option<&OsStr>,
    default: &Path,
) -> Option<PathBuf> {
    if let Some(c) = configured.filter(|c| !c.as_os_str().is_empty()) {
        return Some(c.to_path_buf());
    }
    if let Some(e) = env.filter(|e| !e.is_empty()) {
        return Some(PathBuf::from(e));
    }
    default.is_dir().then(|| default.to_path_buf())
}

/// The `sm_<NN>` directory name of compute capability `cc`.
pub fn sm(cc: (u32, u32)) -> String {
    format!("sm_{}{}", cc.0, cc.1)
}

/// The file in `dir` of model `name`: `<name>` with [`MODEL_EXT`], else
/// the first (by name) `<name>_<digits>` with it.
pub fn model_file(dir: &Path, name: &str) -> Option<PathBuf> {
    let exact = dir.join(format!("{name}{MODEL_EXT}"));
    if exact.is_file() {
        return Some(exact);
    }
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_prefix(name))
                    .and_then(|rest| rest.strip_prefix('_'))
                    .and_then(|rest| rest.strip_suffix(MODEL_EXT))
                    .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        })
        .collect();
    found.sort();
    found.into_iter().next()
}

/// The feature directory of `effect` under `root`.
pub fn feature_dir(root: &Path, effect: Effect) -> PathBuf {
    root.join("features").join(effect.feature())
}

/// The directory of `effect`'s feature library under `root`.
pub fn library_dir(root: &Path, effect: Effect) -> PathBuf {
    feature_dir(root, effect).join("lib")
}

/// Whether `effect`'s feature library is installed under `root` (by its
/// unversioned name, which the SDK opens, or a versioned one).
pub fn has_library(root: &Path, effect: Effect) -> bool {
    let dir = library_dir(root, effect);
    let name = ffi::feature_library(effect.selector());
    std::fs::read_dir(&dir).is_ok_and(|entries| {
        entries.filter_map(|e| e.ok()).any(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n == name || n.starts_with(&format!("{name}."))
        })
    })
}

/// The model directories (`sm_<NN>`) of `effect` under `root` that hold all
/// its models, by name.
pub fn model_dirs(root: &Path, effect: Effect) -> Vec<PathBuf> {
    let models = feature_dir(root, effect).join("models");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&models)
        .map(|it| {
            it.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.is_dir()
                        && p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with("sm_"))
                })
                .filter(|p| has_models(p, effect))
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs
}

fn has_models(dir: &Path, effect: Effect) -> bool {
    std::iter::once(effect.model())
        .chain(effect.extra_models().iter().copied())
        .all(|m| model_file(dir, m).is_some())
}

/// Why `effect` cannot run from the SDK tree at `root` on a GPU of
/// `compute_capability` (`None`: any), in plain words for the user; `None`
/// when it can. `driver`: whether the NVIDIA driver is loaded.
pub fn unavailable(
    root: Option<&Path>,
    compute_capability: Option<(u32, u32)>,
    driver: bool,
    effect: Effect,
) -> Option<String> {
    let Some(root) = root.filter(|r| ffi::core_library(r).is_some()) else {
        return Some(crate::REASON_NO_SDK.into());
    };
    if !driver {
        return Some(crate::REASON_NO_DRIVER.into());
    }
    if let Some(why) = effect.not_installed() {
        let models = match compute_capability {
            Some(cc) => model_dirs(root, effect)
                .iter()
                .any(|d| d.file_name().and_then(|n| n.to_str()) == Some(sm(cc).as_str())),
            None => !model_dirs(root, effect).is_empty(),
        };
        return (!has_library(root, effect) || !models).then(|| why.to_string());
    }
    if !has_library(root, effect) {
        return Some(format!("Maxine {} not installed", effect.label()));
    }
    let dirs = model_dirs(root, effect);
    match compute_capability {
        Some(cc) => {
            let want = sm(cc);
            let found = dirs
                .iter()
                .any(|d| d.file_name().and_then(|n| n.to_str()) == Some(want.as_str()));
            (!found).then(|| format!("No {} models for this GPU ({want})", effect.label()))
        }
        None => dirs
            .is_empty()
            .then(|| format!("No {} models installed", effect.label())),
    }
}

/// Whether the NVIDIA driver is loaded.
pub fn driver_loaded() -> bool {
    Path::new(DRIVER_FILE).exists()
}

/// The model directory of `effect` under `root` for this GPU: that of
/// `compute_capability` when known; else the only one installed; else,
/// with several, that of the GPU the CUDA driver reports (which is GPU
/// work: only from a load).
pub fn models_for_gpu(
    root: &Path,
    effect: Effect,
    compute_capability: Option<(u32, u32)>,
) -> Result<PathBuf, String> {
    let dirs = model_dirs(root, effect);
    let pick = |cc: (u32, u32)| {
        let want = sm(cc);
        dirs.iter()
            .find(|d| d.file_name().and_then(|n| n.to_str()) == Some(want.as_str()))
            .cloned()
            .ok_or_else(|| format!("No {} models for this GPU ({want})", effect.label()))
    };
    match (compute_capability, dirs.len()) {
        (Some(cc), _) => pick(cc),
        (None, 0) => Err(format!("No {} models installed", effect.label())),
        (None, 1) => Ok(dirs[0].clone()),
        (None, _) => pick(ffi::compute_capability()?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"").unwrap();
    }

    #[test]
    fn the_root_is_the_configured_one_then_the_environment_s_then_the_default() {
        let d = tempfile::tempdir().unwrap();
        let missing = d.path().join("missing");
        let c = Path::new("/c");
        let e = OsStr::new("/e");
        assert_eq!(
            resolve_root(Some(c), Some(e), d.path()),
            Some(PathBuf::from("/c"))
        );
        assert_eq!(
            resolve_root(Some(Path::new("")), Some(e), d.path()),
            Some(PathBuf::from("/e"))
        );
        assert_eq!(
            resolve_root(None, Some(OsStr::new("")), d.path()),
            Some(d.path().to_path_buf())
        );
        assert_eq!(resolve_root(None, None, &missing), None);
    }

    #[test]
    fn model_files_are_found_by_short_or_downloaded_name() {
        let d = tempfile::tempdir().unwrap();
        let file = |n: &str| d.path().join(format!("{n}{MODEL_EXT}"));
        touch(&file("model_a_37"));
        touch(&file("model_b_x"));
        assert_eq!(model_file(d.path(), "model_a"), Some(file("model_a_37")));
        assert_eq!(model_file(d.path(), "model_b"), None, "not digits");
        touch(&file("model_b"));
        assert_eq!(model_file(d.path(), "model_b"), Some(file("model_b")));
        assert_eq!(model_file(d.path(), "model_c"), None);
    }

    #[test]
    fn availability_needs_the_core_the_driver_the_feature_and_its_models() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let e = Effect::StudioVoice;
        let why = |cc, driver| unavailable(Some(root), cc, driver, e);
        assert_eq!(why(None, true).as_deref(), Some(crate::REASON_NO_SDK));
        touch(&root.join("nvafx/lib/libnv_audiofx.so.3"));
        assert_eq!(why(None, false).as_deref(), Some(crate::REASON_NO_DRIVER));
        assert_eq!(
            why(None, true).as_deref(),
            Some("Maxine studio voice not installed")
        );
        touch(&root.join("features/studio_voice/lib/libnv_audiofx_studio_voice_low_latency.so.3"));
        assert_eq!(
            why(None, true).as_deref(),
            Some("No studio voice models installed")
        );
        let sm = root.join("features/studio_voice/models/sm_120");
        touch(&sm.join(format!("{}{MODEL_EXT}", e.model())));
        assert_eq!(
            why(None, true).as_deref(),
            Some("No studio voice models installed"),
            "the mic profile model is needed too"
        );
        touch(&sm.join(format!("{MIC_PROFILE_MODEL}_7{MODEL_EXT}")));
        assert_eq!(why(None, true), None);
        assert_eq!(why(Some((12, 0)), true), None);
        assert_eq!(
            why(Some((8, 6)), true).as_deref(),
            Some("No studio voice models for this GPU (sm_86)")
        );
        assert_eq!(
            unavailable(None, None, true, e).as_deref(),
            Some(crate::REASON_NO_SDK)
        );
        assert_eq!(models_for_gpu(root, e, None), Ok(sm.clone()));
        assert_eq!(models_for_gpu(root, e, Some((12, 0))), Ok(sm));
        assert!(models_for_gpu(root, e, Some((8, 9))).is_err());
        assert!(models_for_gpu(root, Effect::Denoiser, None).is_err());
    }

    #[test]
    fn speaker_focus_missing_in_any_part_is_models_not_installed() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let e = Effect::SpeakerFocus;
        let why = |cc| unavailable(Some(root), cc, true, e);
        assert_eq!(why(None).as_deref(), Some(crate::REASON_NO_SDK));
        touch(&root.join("nvafx/lib/libnv_audiofx.so.3"));
        assert_eq!(
            unavailable(Some(root), None, false, e).as_deref(),
            Some(crate::REASON_NO_DRIVER),
            "the driver is still said first"
        );
        assert_eq!(why(None).as_deref(), Some(REASON_MODELS_NOT_INSTALLED));
        let feature = root.join("features/speaker_focus");
        std::fs::create_dir_all(&feature).unwrap();
        assert_eq!(why(None).as_deref(), Some(REASON_MODELS_NOT_INSTALLED));
        touch(&feature.join("lib/libnv_audiofx_speaker_focus.so.3"));
        assert_eq!(why(None).as_deref(), Some(REASON_MODELS_NOT_INSTALLED));
        let sm = feature.join("models/sm_120");
        touch(&sm.join(format!("speaker_focus_48k_3{MODEL_EXT}")));
        assert_eq!(why(None), None);
        assert_eq!(why(Some((12, 0))), None);
        assert_eq!(
            why(Some((8, 6))).as_deref(),
            Some(REASON_MODELS_NOT_INSTALLED)
        );
        std::fs::remove_dir_all(feature.join("lib")).unwrap();
        assert_eq!(why(None).as_deref(), Some(REASON_MODELS_NOT_INSTALLED));
        assert_eq!(models_for_gpu(root, e, None), Ok(sm));
    }
}
