//! What the tests share: the module's library and the stand-in SDK
//! library (`tests/fixtures/mock-afx`), each built once per test binary by
//! cargo into a directory under `CARGO_TARGET_TMPDIR`; a stand-in SDK tree
//! made of them; the module opened from it with the audio kinds, as the
//! audio worker opens it; and child processes of a test binary started
//! with a library search path that leads to a tree's feature libraries, as
//! the worker's drop-in starts the worker (the dynamic linker reads
//! `LD_LIBRARY_PATH` once, at process start).

#![allow(dead_code)] // each test binary uses part of it

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use obc_backend::{StageId, Value};
use obc_module_afx::tree::MODEL_EXT;
use obc_module_host::{AudioSpec, LoadKinds, Manifest, Module};

pub const ID: &str = "br.com.dias.maxine-afx";
pub const DENOISE: &str = "mic-denoise";
pub const DEREVERB: &str = "mic-dereverb";
pub const ENHANCE: &str = "mic-enhance";
pub const FOCUS: &str = "mic-focus";
pub const HOP: usize = 480;
const PACKAGED: &str = "library = \"/usr/lib/omarchy-broadcast/modules/libobc_module_afx.so\"";

pub fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Builds the crate at `manifest` (the package `package` of its workspace,
/// if given) into `target` (`--locked`: its `Cargo.lock` as it is) and
/// returns the library `lib` it made.
fn build(manifest: &Path, package: Option<&str>, target: &str, lib: &str) -> PathBuf {
    let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join(target);
    let mut cmd = Command::new(env!("CARGO"));
    cmd.args(["build", "--quiet", "--locked"]);
    if let Some(package) = package {
        cmd.args(["-p", package]);
    }
    let status = cmd
        .arg("--manifest-path")
        .arg(manifest)
        .arg("--target-dir")
        .arg(&target)
        // Not the variables of the outer build, whose target this is not.
        .env_remove("CARGO_TARGET_DIR")
        .status()
        .expect("run cargo");
    assert!(status.success(), "building {} failed", manifest.display());
    let path = target.join("debug").join(lib);
    assert!(path.is_file(), "{} was not built", path.display());
    path
}

/// The variable that hands the built module library to a child process.
const LIBRARY_ENV: &str = "OBC_AFX_TEST_LIBRARY";
/// The variable that names the test a child process runs.
const CHILD_ENV: &str = "OBC_AFX_TEST_CHILD";
/// The variable that names the child's stand-in tree.
const ROOT_ENV: &str = "OBC_AFX_TEST_ROOT";

/// `libobc_module_afx.so`.
pub fn library() -> PathBuf {
    static LIB: OnceLock<PathBuf> = OnceLock::new();
    LIB.get_or_init(|| {
        if let Some(p) = std::env::var_os(LIBRARY_ENV) {
            return PathBuf::from(p);
        }
        // From the repository's workspace, whose lock file it shares.
        build(
            &crate_dir().join("../../Cargo.toml"),
            Some("obc-module-afx"),
            "afx-module",
            "libobc_module_afx.so",
        )
    })
    .clone()
}

/// The stand-in `libnv_audiofx.so`.
pub fn mock_library() -> PathBuf {
    static LIB: OnceLock<PathBuf> = OnceLock::new();
    LIB.get_or_init(|| {
        build(
            &crate_dir().join("tests/fixtures/mock-afx/Cargo.toml"),
            None,
            "mock-afx",
            "libnv_audiofx.so",
        )
    })
    .clone()
}

/// The shipped manifest, installed in `dir` with the built library.
pub fn install(dir: &Path) -> Manifest {
    let text = std::fs::read_to_string(crate_dir().join("maxine-afx.toml")).unwrap();
    let installed = text.replace(PACKAGED, &format!("library = \"{}\"", library().display()));
    assert_ne!(text, installed, "the manifest's library line moved");
    let path = dir.join("maxine-afx.toml");
    std::fs::write(&path, installed).unwrap();
    Manifest::load(&path).unwrap()
}

/// The features of the stand-in tree: `(feature dir, selector, models)`.
pub const FEATURES: [(&str, &str, &[&str]); 5] = [
    ("denoiser", "denoiser", &["denoiser_48k", "denoiser_v2_48k"]),
    ("dereverb", "dereverb", &["dereverb_48k"]),
    (
        "dereverb_denoiser",
        "dereverb_denoiser",
        &["dereverb_denoiser_48k_77"],
    ),
    (
        "studio_voice",
        "studio_voice_low_latency",
        &[
            "studio_voice_low_latency_48k",
            "studio_voice_mic_profile_48k",
        ],
    ),
    // Speaker focus, by NVIDIA's download layout (not installed by
    // default: tests remove it to see the stage unavailable).
    ("speaker_focus", "speaker_focus", &["speaker_focus_48k"]),
];

/// A stand-in SDK tree in `root`: the stand-in library as the core and as
/// each feature library (with its unversioned link), and an empty file per
/// model, for GPUs `sm_120`.
pub struct Tree {
    pub root: PathBuf,
}

impl Tree {
    pub fn new(root: &Path) -> Tree {
        let mock = mock_library();
        let core = root.join("nvafx/lib");
        std::fs::create_dir_all(&core).unwrap();
        std::fs::copy(&mock, core.join("libnv_audiofx.so.3")).unwrap();
        std::os::unix::fs::symlink("libnv_audiofx.so.3", core.join("libnv_audiofx.so")).unwrap();
        for (feature, selector, models) in FEATURES {
            let lib = root.join("features").join(feature).join("lib");
            std::fs::create_dir_all(&lib).unwrap();
            let name = format!("libnv_audiofx_{selector}.so");
            if !lib.join(format!("{name}.3")).exists() {
                std::fs::copy(&mock, lib.join(format!("{name}.3"))).unwrap();
            }
            std::os::unix::fs::symlink(format!("{name}.3"), lib.join(&name)).unwrap();
            let sm = root.join("features").join(feature).join("models/sm_120");
            std::fs::create_dir_all(&sm).unwrap();
            for m in models {
                std::fs::write(sm.join(format!("{m}{MODEL_EXT}")), b"mock").unwrap();
            }
        }
        Tree {
            root: root.to_path_buf(),
        }
    }

    /// The tree at `root`, made earlier (by the parent of a child process).
    pub fn at(root: &Path) -> Tree {
        Tree {
            root: root.to_path_buf(),
        }
    }

    /// The library search path the worker's drop-in would set for this
    /// tree: each feature's `lib` directory.
    pub fn search_path(&self) -> OsString {
        let mut path = OsString::new();
        for (i, (feature, _, _)) in FEATURES.iter().enumerate() {
            if i > 0 {
                path.push(":");
            }
            path.push(self.root.join("features").join(feature).join("lib"));
        }
        path
    }

    /// Writes the stand-in's `mock-afx.conf`.
    pub fn conf(&self, text: &str) {
        std::fs::write(self.root.join("nvafx/lib/mock-afx.conf"), text).unwrap();
    }

    /// The stand-in's calls so far, one line each.
    pub fn log(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.join("nvafx/lib/mock-afx.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The calls whose line starts with `prefix`.
    pub fn calls(&self, prefix: &str) -> Vec<String> {
        self.log()
            .into_iter()
            .filter(|l| l.starts_with(prefix))
            .collect()
    }

    /// Forgets the calls so far.
    pub fn clear_log(&self) {
        let _ = std::fs::remove_file(self.root.join("nvafx/lib/mock-afx.log"));
    }
}

pub fn s(id: &str) -> StageId {
    StageId::new(id).unwrap()
}

/// A request of `stages` with `params` (`(stage, key, value)`).
pub fn spec(stages: &[&str], params: &[(&str, &str, Value)]) -> AudioSpec {
    AudioSpec {
        stages: stages.iter().map(|x| s(x)).collect(),
        sample_rate: 48_000,
        channels_in: 1,
        channels_out: 1,
        frame_samples: 480,
        params: params
            .iter()
            .map(|(st, k, v)| (s(st), k.to_string(), v.clone()))
            .collect(),
    }
}

/// The module, opened for the audio worker from manifests in `dir`, with
/// the configuration `config`.
pub fn open(dir: &Path, config: serde_json::Value) -> Module {
    Module::open_for(
        &install(dir),
        &config,
        obc_module_host::stderr_log(),
        LoadKinds::Audio,
    )
    .unwrap()
}

/// A tone of `n` samples at `amp`.
pub fn tone(n: usize, amp: f32) -> Vec<f32> {
    (0..n)
        .map(|i| amp * (i as f32 * 2.0 * std::f32::consts::PI * 220.0 / 48_000.0).sin())
        .collect()
}

/// The tree root, when this process is the child that runs `test`.
pub fn child_root(test: &str) -> Option<PathBuf> {
    if std::env::var(CHILD_ENV).ok()? != test {
        return None;
    }
    std::env::var_os(ROOT_ENV).map(PathBuf::from)
}

/// Whether this process is a child that runs a test.
pub fn is_child() -> bool {
    std::env::var_os(CHILD_ENV).is_some()
}

/// Runs `test` of this test binary in a child process over the tree at
/// `root`, started with `LD_LIBRARY_PATH` = `ld` (before what this process
/// has), the variables `env`, and a working directory that holds no
/// library; asserts that it passed and returns what it printed.
pub fn run_child(test: &str, root: &Path, ld: &OsStr, env: &[(&str, &str)]) -> String {
    let mut path = ld.to_os_string();
    if let Some(old) = std::env::var_os("LD_LIBRARY_PATH").filter(|v| !v.is_empty()) {
        path.push(":");
        path.push(old);
    }
    let cwd = tempfile::tempdir().unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .current_dir(cwd.path())
        .env("LD_LIBRARY_PATH", path)
        .env(CHILD_ENV, test)
        .env(ROOT_ENV, root)
        .env(LIBRARY_ENV, library())
        .envs(env.iter().copied())
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "the child of {test} failed:\n{text}");
    assert!(
        text.contains("1 passed"),
        "the child of {test} ran no test:\n{text}"
    );
    text
}
