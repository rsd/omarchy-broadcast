//! Which features of the SDK are installed, from the filesystem alone.
//!
//! The SDK installs each feature as a directory `features/<dir>` with its
//! library in `features/<dir>/lib`, and puts the TensorRT models of every
//! feature together in `lib/models`, one file per model and GPU
//! architecture, named after the feature's model with the architecture's
//! compute capability as a field of the name (`Denoise-agg-120.engine.trtpkg`
//! is a denoising model for compute capability 12.0). The installer
//! downloads the models for one architecture only, so a tree copied from
//! another machine can have the libraries and no model for this GPU. The
//! `features/<dir>/*_dependencies.txt` files name other features a feature
//! needs, not its models (they are empty for every feature of SDK 1.3), so
//! the feature-to-model mapping is the table below.
//!
//! [`feature_availability`] reads directories and nothing else: no CUDA
//! call, no `dlopen`, so it never wakes a runtime-suspended GPU
//! (`docs/adr/0017-cuda-checkpoint-parking.md`).

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;

use obc_backend::Availability;

/// A feature of the SDK that is installed separately: a feature directory
/// with its library and its models. Not every feature is used by the chain
/// yet; the list says what the backend could offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Feature {
    /// The green-screen segmentation: the mask of `blur` and `replace`.
    GreenScreen,
    /// The background blur of `blur`.
    BackgroundBlur,
    /// Video noise removal (the `denoise` stage).
    Denoise,
    /// Relighting of the foreground through the green-screen matte.
    Relighting,
    /// Video super resolution (upscaling, and same-size denoise and
    /// deblur modes).
    VideoSuperResolution,
    /// A lightweight upscaler.
    Upscale,
    /// Video frame generation (frames interpolated between two inputs).
    VideoFrameGeneration,
    /// TrueHDR: SDR to HDR (PQ, BT.2020) inverse tone mapping.
    TrueHdr,
}

impl Feature {
    /// Every feature, in declaration order.
    pub const ALL: [Feature; 8] = [
        Feature::GreenScreen,
        Feature::BackgroundBlur,
        Feature::Denoise,
        Feature::Relighting,
        Feature::VideoSuperResolution,
        Feature::Upscale,
        Feature::VideoFrameGeneration,
        Feature::TrueHdr,
    ];

    /// A stable lower-case name for logs and interfaces.
    pub fn as_str(self) -> &'static str {
        match self {
            Feature::GreenScreen => "green-screen",
            Feature::BackgroundBlur => "background-blur",
            Feature::Denoise => "denoise",
            Feature::Relighting => "relighting",
            Feature::VideoSuperResolution => "video-super-resolution",
            Feature::Upscale => "upscale",
            Feature::VideoFrameGeneration => "video-frame-generation",
            Feature::TrueHdr => "true-hdr",
        }
    }
}

impl fmt::Display for Feature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A family of model files: those whose stem (the name without
/// [`MODEL_SUFFIX`]) starts with `prefix` and whose remaining fields,
/// split at `sep`, carry the compute capability at `sm`.
struct Models {
    prefix: &'static str,
    sep: char,
    sm: Field,
}

/// Where the compute capability is among the fields after the prefix.
enum Field {
    /// The n-th field (0-based).
    At(usize),
    /// The last field.
    Last,
}

/// What a feature needs on disk.
struct Spec {
    /// The directory under `features/`.
    dir: &'static str,
    /// The library in `features/<dir>/lib`.
    library: &'static str,
    /// The model families it needs, at least one file of each for this GPU.
    /// Empty for a feature that ships no model files.
    models: &'static [Models],
}

/// The suffix of every model file.
const MODEL_SUFFIX: &str = ".engine.trtpkg";

/// The green screen: `AIGS_<input size>_<sm>_m<mode>[_<batch sizes>]`, one
/// file per quality mode, any of which the effect can use.
const GREEN_SCREEN_MODELS: &[Models] = &[Models {
    prefix: "AIGS_",
    sep: '_',
    sm: Field::At(1),
}];

/// Denoising: `Denoise-<agg|con>-<sm>`, one model per strength.
const DENOISE_MODELS: &[Models] = &[Models {
    prefix: "Denoise-",
    sep: '-',
    sm: Field::Last,
}];

/// Relighting: `relight_0thframe_perf_<sm>` for the first frame of a video
/// and `relight_nthframe_perf_<sm>` for the following ones; both are needed.
const RELIGHTING_MODELS: &[Models] = &[
    Models {
        prefix: "relight_0thframe_",
        sep: '_',
        sm: Field::Last,
    },
    Models {
        prefix: "relight_nthframe_",
        sep: '_',
        sm: Field::Last,
    },
];

/// The SDK feature directory of `feature` (under the root's `features/`).
pub fn feature_dir(feature: Feature) -> &'static str {
    spec(feature).dir
}

fn spec(feature: Feature) -> Spec {
    let (dir, library, models) = match feature {
        Feature::GreenScreen => (
            "nvvfxgreenscreen",
            "libnvVFXGreenScreen.so",
            GREEN_SCREEN_MODELS,
        ),
        // The blur filters through the green screen's mask; it has no model.
        Feature::BackgroundBlur => ("nvvfxbackgroundblur", "libnvVFXBackgroundBlur.so", &[][..]),
        Feature::Denoise => ("nvvfxdenoising", "libnvVFXDenoising.so", DENOISE_MODELS),
        Feature::Relighting => (
            "nvvfxrelighting",
            "libnvVFXRelighting.so",
            RELIGHTING_MODELS,
        ),
        // These ship no model files: the library alone is the feature.
        Feature::VideoSuperResolution => {
            ("nvvfxvideosuperres", "libnvVFXVideoSuperRes.so", &[][..])
        }
        Feature::Upscale => ("nvvfxupscale", "libnvVFXUpscale.so", &[][..]),
        Feature::VideoFrameGeneration => (
            "nvvfxvideoframegeneration",
            "libnvVFXVideoFrameGeneration.so",
            &[][..],
        ),
        Feature::TrueHdr => ("nvvfxtruehdr", "libnvVFXTrueHDR.so", &[][..]),
    };
    Spec {
        dir,
        library,
        models,
    }
}

impl Models {
    /// The compute capability field of the model file `name`, if it is one
    /// of this family.
    fn sm_of<'a>(&self, name: &'a str) -> Option<&'a str> {
        let rest = name.strip_suffix(MODEL_SUFFIX)?.strip_prefix(self.prefix)?;
        let mut fields = rest.split(self.sep);
        let sm = match self.sm {
            Field::At(n) => fields.nth(n)?,
            Field::Last => fields.next_back()?,
        };
        (!sm.is_empty() && sm.bytes().all(|b| b.is_ascii_digit())).then_some(sm)
    }

    /// The family's name for messages: its prefix without the separator.
    fn name(&self) -> &'static str {
        self.prefix.trim_end_matches(self.sep)
    }
}

/// Whether each [`Feature`] is installed in the SDK tree at `root` (the
/// root [`crate::effective_root`] resolves; with no root, nothing can be
/// checked and the caller should treat every feature as unknown), in
/// [`Feature::ALL`] order.
///
/// A feature is [`Availability::Installed`] when its library
/// `features/<dir>/lib/<library>` exists and, for a feature with models,
/// `lib/models` holds at least one model of each family it needs for
/// `compute_capability` (`(major, minor)`, as CUDA reports it; `(12, 0)`
/// matches models named for `120`). With `None`, a model for any
/// architecture counts. Otherwise the reason names what is missing.
///
/// Reads directories only: no CUDA call and no `dlopen`, so it is safe while
/// the GPU is runtime-suspended. Installed does not promise that the feature
/// loads: a damaged library or a driver too old for it shows only then.
pub fn feature_availability(
    root: &Path,
    compute_capability: Option<(u32, u32)>,
) -> Vec<(Feature, Availability)> {
    let model_dir = root.join("lib").join("models");
    let models: Vec<String> = std::fs::read_dir(&model_dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    let sm = compute_capability.map(|(major, minor)| format!("{major}{minor}"));
    Feature::ALL
        .into_iter()
        .map(|f| (f, availability(root, &model_dir, &models, sm.as_deref(), f)))
        .collect()
}

fn availability(
    root: &Path,
    model_dir: &Path,
    models: &[String],
    sm: Option<&str>,
    feature: Feature,
) -> Availability {
    let spec = spec(feature);
    let library = root
        .join("features")
        .join(spec.dir)
        .join("lib")
        .join(spec.library);
    if !library.is_file() {
        return Availability::NotInstalled(format!(
            "the feature library {} is not installed",
            library.display()
        ));
    }
    for family in spec.models {
        let found: BTreeSet<&str> = models.iter().filter_map(|m| family.sm_of(m)).collect();
        let usable = match sm {
            Some(sm) => found.contains(sm),
            None => !found.is_empty(),
        };
        if usable {
            continue;
        }
        let gpu = match sm {
            Some(sm) => format!(" for this GPU (sm_{sm})"),
            None => String::new(),
        };
        let others = if found.is_empty() {
            String::new()
        } else {
            let list: Vec<String> = found.iter().map(|s| format!("sm_{s}")).collect();
            format!("; it has them only for {}", list.join(", "))
        };
        return Availability::NotInstalled(format!(
            "no {} model{gpu} in {}{others}",
            family.name(),
            model_dir.display()
        ));
    }
    Availability::Installed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A fake SDK tree with the libraries of `features` and the model files
    /// `models`.
    fn tree(features: &[Feature], models: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for &f in features {
            let s = spec(f);
            let lib = dir.path().join("features").join(s.dir).join("lib");
            std::fs::create_dir_all(&lib).unwrap();
            std::fs::write(lib.join(s.library), b"not a library").unwrap();
        }
        let m = dir.path().join("lib/models");
        std::fs::create_dir_all(&m).unwrap();
        for name in models {
            std::fs::write(m.join(name), b"").unwrap();
        }
        dir
    }

    #[test]
    fn features_have_stable_names() {
        let names: Vec<&str> = Feature::ALL.iter().map(|f| f.as_str()).collect();
        assert_eq!(
            names,
            [
                "green-screen",
                "background-blur",
                "denoise",
                "relighting",
                "video-super-resolution",
                "upscale",
                "video-frame-generation",
                "true-hdr"
            ]
        );
        assert_eq!(Feature::Relighting.to_string(), "relighting");
        fn _send_clone<T: Send + Sync + Clone>() {}
        _send_clone::<Feature>();
    }

    fn of(all: &[(Feature, Availability)], f: Feature) -> &Availability {
        &all.iter().find(|(g, _)| *g == f).unwrap().1
    }

    /// The model files of SDK 1.3 for compute capability 12.0.
    const SM120: &[&str] = &[
        "AIGS_288x512_120_m0_1_4_8.engine.trtpkg",
        "AIGS_288x512_120_m0.engine.trtpkg",
        "AIGS_288x512_120_m1.engine.trtpkg",
        "Denoise-agg-120.engine.trtpkg",
        "Denoise-con-120.engine.trtpkg",
        "relight_0thframe_perf_120.engine.trtpkg",
        "relight_nthframe_perf_120.engine.trtpkg",
    ];

    #[test]
    fn every_feature_is_reported_once_in_order() {
        let t = tree(&[], &[]);
        let all = feature_availability(t.path(), None);
        let features: Vec<Feature> = all.iter().map(|(f, _)| *f).collect();
        assert_eq!(features, Feature::ALL);
        for (f, a) in &all {
            let r = a
                .reason()
                .unwrap_or_else(|| panic!("{f} installed in an empty tree"));
            assert!(r.contains(feature_dir(*f)), "{f}: {r}");
            assert!(r.contains("is not installed"), "{f}: {r}");
        }
    }

    #[test]
    fn library_and_models_for_this_gpu_make_a_feature_installed() {
        let t = tree(&Feature::ALL, SM120);
        for cc in [Some((12, 0)), None] {
            let all = feature_availability(t.path(), cc);
            for (f, a) in &all {
                assert_eq!(*a, Availability::Installed, "{f} with {cc:?}");
            }
        }
    }

    #[test]
    fn models_only_for_another_gpu_are_not_enough() {
        let t = tree(&Feature::ALL, SM120);
        let all = feature_availability(t.path(), Some((8, 6)));
        for f in [Feature::GreenScreen, Feature::Denoise, Feature::Relighting] {
            let r = of(&all, f).reason().unwrap_or_else(|| panic!("{f}"));
            assert!(r.contains("for this GPU (sm_86)"), "{f}: {r}");
            assert!(r.contains("it has them only for sm_120"), "{f}: {r}");
            assert!(r.contains("lib/models"), "{f}: {r}");
        }
        // Features that ship no model files depend on the library only.
        for f in [
            Feature::BackgroundBlur,
            Feature::VideoSuperResolution,
            Feature::Upscale,
            Feature::VideoFrameGeneration,
            Feature::TrueHdr,
        ] {
            assert!(of(&all, f).is_installed(), "{f}");
        }
        // Several architectures: the one of this GPU is picked out.
        let t = tree(
            &[Feature::Denoise],
            &[
                "Denoise-agg-86.engine.trtpkg",
                "Denoise-agg-120.engine.trtpkg",
            ],
        );
        for cc in [(8, 6), (12, 0)] {
            let all = feature_availability(t.path(), Some(cc));
            assert!(of(&all, Feature::Denoise).is_installed(), "{cc:?}");
        }
        let all = feature_availability(t.path(), Some((7, 5)));
        let r = of(&all, Feature::Denoise).reason().unwrap();
        assert!(r.contains("only for sm_120, sm_86"), "{r}");
    }

    #[test]
    fn a_library_without_models_is_not_installed() {
        let t = tree(&Feature::ALL, &[]);
        for cc in [Some((12, 0)), None] {
            let all = feature_availability(t.path(), cc);
            for (f, name) in [
                (Feature::GreenScreen, "AIGS"),
                (Feature::Denoise, "Denoise"),
                (Feature::Relighting, "relight_0thframe"),
            ] {
                let r = of(&all, f).reason().unwrap_or_else(|| panic!("{f}"));
                assert!(r.starts_with(&format!("no {name} model")), "{f}: {r}");
                assert!(!r.contains("only for"), "{f}: {r}");
            }
            assert!(of(&all, Feature::BackgroundBlur).is_installed());
        }
        // A model directory that does not exist is the same as an empty one.
        let t = tree(&[Feature::Denoise], &[]);
        std::fs::remove_dir(t.path().join("lib/models")).unwrap();
        let all = feature_availability(t.path(), None);
        assert!(!of(&all, Feature::Denoise).is_installed());
    }

    #[test]
    fn relighting_needs_both_of_its_models() {
        let t = tree(
            &[Feature::Relighting],
            &["relight_0thframe_perf_120.engine.trtpkg"],
        );
        let all = feature_availability(t.path(), Some((12, 0)));
        let r = of(&all, Feature::Relighting).reason().unwrap();
        assert!(r.starts_with("no relight_nthframe model"), "{r}");
    }

    #[test]
    fn models_without_their_library_are_not_installed() {
        let t = tree(&[], SM120);
        let all = feature_availability(t.path(), None);
        let r = of(&all, Feature::Denoise).reason().unwrap();
        let lib: PathBuf = ["features", "nvvfxdenoising", "lib", "libnvVFXDenoising.so"]
            .iter()
            .collect();
        assert!(r.contains(&lib.display().to_string()), "{r}");
    }

    #[test]
    fn model_names_give_their_architecture() {
        let gs = &GREEN_SCREEN_MODELS[0];
        assert_eq!(gs.sm_of("AIGS_288x512_120_m0.engine.trtpkg"), Some("120"));
        assert_eq!(
            gs.sm_of("AIGS_288x512_86_m3_1_4_8.engine.trtpkg"),
            Some("86")
        );
        assert_eq!(gs.sm_of("AIGS_288x512_120_m0.onnx"), None);
        assert_eq!(gs.sm_of("AIGS_288x512.engine.trtpkg"), None);
        let dn = &DENOISE_MODELS[0];
        assert_eq!(dn.sm_of("Denoise-con-75.engine.trtpkg"), Some("75"));
        assert_eq!(dn.sm_of("Denoise-con-.engine.trtpkg"), None);
        assert_eq!(dn.sm_of("Denoise-con-x1.engine.trtpkg"), None);
        assert_eq!(dn.sm_of("AIGS_288x512_120_m0.engine.trtpkg"), None);
        let rl = &RELIGHTING_MODELS[1];
        assert_eq!(
            rl.sm_of("relight_nthframe_perf_100.engine.trtpkg"),
            Some("100")
        );
        assert_eq!(rl.sm_of("relight_0thframe_perf_100.engine.trtpkg"), None);
        assert_eq!(rl.name(), "relight_nthframe");
    }

    #[test]
    fn the_directories_are_the_sdk_feature_names() {
        let dirs: Vec<&str> = Feature::ALL.iter().map(|f| feature_dir(*f)).collect();
        assert_eq!(
            dirs,
            [
                "nvvfxgreenscreen",
                "nvvfxbackgroundblur",
                "nvvfxdenoising",
                "nvvfxrelighting",
                "nvvfxvideosuperres",
                "nvvfxupscale",
                "nvvfxvideoframegeneration",
                "nvvfxtruehdr"
            ]
        );
    }
}
