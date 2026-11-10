//! The SDK found from its root alone, without `LD_LIBRARY_PATH`. Ignored: it
//! needs the NVIDIA Maxine Video Effects SDK, an NVIDIA GPU and, on a
//! distribution the SDK does not list, the os-release override; no camera.
//!
//! Each test runs its body in a child process of this test binary, because
//! the loader reads `LD_LIBRARY_PATH` and `LD_PRELOAD` only when a process
//! starts. The child gets no `LD_LIBRARY_PATH`, the os-release shim as
//! `LD_PRELOAD` and `MAXINE_OS_RELEASE` from this process, and the SDK root
//! one way only: as `MAXINE_VFX_ROOT`; as the configured root
//! (`set_configured_root`, what the daemon's `[effect] maxine_root` sets)
//! with `MAXINE_VFX_ROOT` removed; or not at all, the end user's setup with
//! the `maxine-vfx-sdk-local` package and nothing set, where the factory
//! must find the package's `/opt/nvidia/maxine/videofx` itself. In the
//! child, blur and the denoise stage each load and run a frame, and
//! `/proc/self/maps` must show the SDK's libraries mapped from that root and
//! none of its bundled libraries from anywhere else.
//!
//! For the first two, the root is the one this process would use
//! (`MAXINE_VFX_ROOT`, else the package's `/opt/nvidia/maxine/videofx`).
//! The shim is `OBC_TEST_PRELOAD` (an absolute path), else the one this
//! process was started with:
//!
//! ```text
//! MAXINE_VFX_ROOT=/path/to/VideoFX MAXINE_OS_RELEASE=/path/to/fake-os-release \
//! OBC_TEST_PRELOAD=/path/to/os-release-shim.so \
//!   cargo test -p obc-backend-maxine --test sdk_root -- --ignored --test-threads=1 --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use obc_backend::{
    BackendFactory, ColourSpec, EffectKind, FrameParams, GpuContext, InferenceBackend, LoadRequest,
};
use obc_backend_maxine::{DEFAULT_ROOT, MaxineFactory, ROOT_ENV, set_configured_root};
use obc_gpu::{ContextGuard, DeviceNv12Buffer, OwnedContext, Stream};

const W: u32 = 1280;
const H: u32 = 720;

/// Which test body a child process of this binary runs.
const CHILD_VAR: &str = "OBC_MAXINE_ROOT_TEST_CHILD";
/// The root a child of the configured-root test configures.
const CONFIGURED_VAR: &str = "OBC_MAXINE_ROOT_TEST_CONFIGURED";
/// The os-release shim the children are started with.
const PRELOAD_VAR: &str = "OBC_TEST_PRELOAD";
/// The os-release file the shim serves.
const OS_RELEASE_VAR: &str = "MAXINE_OS_RELEASE";

/// The SDK libraries that must be mapped from the root once blur and the
/// denoise stage have run: the SDK itself, the three features, and a
/// library of each bundled toolkit they need.
const FROM_ROOT: [&str; 10] = [
    "libNVCVImage.so",
    "libVideoFX.so",
    "libVideoFXLocal.so",
    "libnvVFXGreenScreen.so",
    "libnvVFXBackgroundBlur.so",
    "libnvVFXDenoising.so",
    "libcudart.so.12",
    "libnppc.so.12",
    "libcudnn.so.9",
    "libnvinfer.so.10",
];

/// The os-release shim this process was started with, if any.
fn preloaded_shim() -> Option<String> {
    mapped_files()
        .into_iter()
        .find(|p| p.to_string_lossy().ends_with("os-release-shim.so"))
        .map(|p| p.display().to_string())
}

/// Every file mapped into this process.
fn mapped_files() -> Vec<PathBuf> {
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    let mut files: Vec<PathBuf> = maps
        .lines()
        .filter_map(|l| l.split_whitespace().nth(5))
        .filter(|p| p.starts_with('/'))
        .map(PathBuf::from)
        .collect();
    files.sort();
    files.dedup();
    files
}

/// Runs test `name` again in a child process with the SDK root given one
/// way (`env`), no `LD_LIBRARY_PATH` and the shim preloaded, and fails if
/// the child fails. Returns `false` in the child itself, which then runs
/// the body.
fn in_child(name: &str, env: &[(&str, Option<&Path>)]) -> bool {
    if std::env::var(CHILD_VAR).as_deref() == Ok(name) {
        return false;
    }
    let shim = std::env::var(PRELOAD_VAR)
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(preloaded_shim)
        .unwrap_or_else(|| {
            panic!(
                "set {PRELOAD_VAR} to the absolute path of the os-release shim \
                 (omarchy-nvidia-sdks tools/os-release-shim)"
            )
        });
    assert!(
        Path::new(&shim).is_absolute(),
        "{PRELOAD_VAR}={shim} is not absolute"
    );
    assert!(
        std::env::var_os(OS_RELEASE_VAR).is_some_and(|v| !v.is_empty()),
        "set {OS_RELEASE_VAR} to the os-release file the shim serves"
    );
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([
        name,
        "--exact",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(CHILD_VAR, name)
    .env_remove("LD_LIBRARY_PATH")
    .env_remove(ROOT_ENV)
    .env_remove(CONFIGURED_VAR)
    .env("LD_PRELOAD", &shim);
    for (k, v) in env {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .spawn()
        .expect("re-run this test binary");
    let mut out = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut out, &mut s);
        s
    });
    let limit = Duration::from_secs(300);
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{name}: the child process did not exit within {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = reader.join().unwrap();
    print!("{stdout}");
    assert!(
        status.success(),
        "{name}: the child process failed: {status}"
    );
    assert!(stdout.contains("1 passed"), "{name}: the child ran no test");
    true
}

/// The SDK root this process would use; the test cannot run without one.
fn sdk_root() -> PathBuf {
    obc_backend_maxine::effective_root().unwrap_or_else(|| {
        panic!(
            "this test needs the Maxine SDK: set {ROOT_ENV} to the SDK directory, or install \
             the maxine-vfx-sdk-local package"
        )
    })
}

/// A horizontal luma ramp with neutral chroma, packed NV12.
fn ramp() -> Vec<u8> {
    let (w, h) = (W as usize, H as usize);
    let mut nv12 = vec![128u8; w * h * 3 / 2];
    for y in 0..h {
        for x in 0..w {
            nv12[y * w + x] = (32 + x * 180 / w) as u8;
        }
    }
    nv12
}

/// In a created context: loads `effect` (with the denoise stage when
/// `denoise`) and runs one frame, which must not come out flat.
fn one_frame(f: &MaxineFactory, effect: EffectKind, denoise: bool) {
    let label = format!("{effect:?}{}", if denoise { " + denoise" } else { "" });
    let ctx = OwnedContext::create(0).unwrap();
    // SAFETY: `ctx` outlives the guard; everything below drops first.
    let _g = unsafe { ContextGuard::push(ctx.raw()) }.unwrap();
    let stream = Stream::new(true).unwrap();
    let gpu = GpuContext {
        context: ctx.raw(),
        stream: stream.raw(),
    };
    let src = DeviceNv12Buffer::alloc(W, H).unwrap();
    let dst = DeviceNv12Buffer::alloc(W, H).unwrap();
    src.upload_packed(&ramp()).unwrap();
    let mut backend = f.create_maxine().unwrap();
    let req = LoadRequest {
        effect,
        denoise,
        width: W,
        height: H,
        colour: ColourSpec::LIMITED_BT601,
        background: None,
    };
    backend
        .load(&gpu, &req)
        .unwrap_or_else(|e| panic!("{label}: load failed: {e}"));
    backend
        .process(&gpu, &FrameParams::default(), &src.frame(), &dst.frame())
        .unwrap_or_else(|e| panic!("{label}: process failed: {e}"));
    stream.synchronize().unwrap();
    let out = dst.download_packed().unwrap();
    let luma = &out[..(W * H) as usize];
    assert!(
        luma.iter().any(|&v| v != luma[0]),
        "{label}: the output is flat"
    );
    println!("ROOT {label}: loaded and ran a frame");
    backend.unload();
}

/// The child's body: the environment is what the parent set, the effects
/// run, and the SDK's libraries came from `root`.
fn effects_run_from(f: &MaxineFactory, root: &Path) {
    assert_eq!(std::env::var_os("LD_LIBRARY_PATH"), None);
    assert!(
        preloaded_shim().is_some(),
        "the os-release shim is not loaded"
    );
    assert_eq!(f.root(), Some(root));
    f.probe()
        .unwrap_or_else(|e| panic!("Maxine probe failed: {e}"));
    one_frame(f, EffectKind::Blur, false);
    one_frame(f, EffectKind::None, true);

    let root = root.canonicalize().unwrap();
    let files = mapped_files();
    for f in files.iter().filter(|p| p.starts_with(&root)) {
        println!("ROOT mapped from the root: {}", f.display());
    }
    for name in FROM_ROOT {
        let maps: Vec<&PathBuf> = files
            .iter()
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n == name || n.starts_with(&format!("{name}.")))
            })
            .collect();
        assert!(!maps.is_empty(), "{name} is not mapped");
        for p in maps {
            assert!(p.starts_with(&root), "{name} mapped from {}", p.display());
        }
    }
    // The JIT linker TensorRT opens by name is loaded first from the root
    // too (it is not needed by every model, so it may be absent).
    match files.iter().find(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("libnvJitLink.so"))
    }) {
        Some(p) => {
            assert!(
                p.starts_with(&root),
                "libnvJitLink mapped from {}",
                p.display()
            );
            println!("ROOT libnvJitLink mapped from {}", p.display());
        }
        None => println!("ROOT libnvJitLink not mapped"),
    }
    // No library the SDK bundles is mapped from anywhere else (a second
    // copy beside the root's).
    let bundled = ["lib", "external/cuda/lib", "external/tensorrt/lib"];
    for p in files.iter().filter(|p| !p.starts_with(&root)) {
        let Some(name) = p.file_name() else { continue };
        for dir in bundled {
            assert!(
                !root.join(dir).join(name).exists(),
                "{} is mapped beside the root's {dir}/{}",
                p.display(),
                name.to_string_lossy()
            );
        }
    }
}

#[test]
#[ignore = "needs the NVIDIA Maxine VFX SDK, the os-release shim (OBC_TEST_PRELOAD) and override (MAXINE_OS_RELEASE) and an NVIDIA GPU"]
fn maxine_effects_run_from_the_variable_alone() {
    const NAME: &str = "maxine_effects_run_from_the_variable_alone";
    if std::env::var(CHILD_VAR).as_deref() != Ok(NAME) {
        let root = sdk_root();
        assert!(in_child(NAME, &[(ROOT_ENV, Some(&root))]));
        return;
    }
    let root = PathBuf::from(std::env::var_os(ROOT_ENV).expect("the parent sets the root"));
    effects_run_from(&MaxineFactory::from_env(), &root);
}

#[test]
#[ignore = "needs the NVIDIA Maxine VFX SDK, the os-release shim (OBC_TEST_PRELOAD) and override (MAXINE_OS_RELEASE) and an NVIDIA GPU"]
fn maxine_effects_run_from_the_configured_root_alone() {
    const NAME: &str = "maxine_effects_run_from_the_configured_root_alone";
    if std::env::var(CHILD_VAR).as_deref() != Ok(NAME) {
        let root = sdk_root();
        assert!(in_child(NAME, &[(CONFIGURED_VAR, Some(&root))]));
        return;
    }
    assert_eq!(std::env::var_os(ROOT_ENV), None);
    let root = PathBuf::from(std::env::var_os(CONFIGURED_VAR).expect("the parent sets the root"));
    set_configured_root(Some(root.clone()));
    effects_run_from(&MaxineFactory::from_env(), &root);
    // The SDK is now loaded from this root: another root is refused, the
    // same one is not.
    assert_eq!(obc_backend_maxine::root_conflict(Some(&root)), None);
    let other = Path::new("/nonexistent/other/sdk");
    let why = obc_backend_maxine::root_conflict(Some(other)).expect("another root conflicts");
    assert!(why.contains("Restart the process"), "{why}");
    let e = MaxineFactory::with_root(Some(other.into()))
        .probe()
        .unwrap_err();
    assert!(
        e.to_string().contains("already loaded in this process"),
        "{e}"
    );
}

#[test]
#[ignore = "needs the maxine-vfx-sdk-local package (its /opt/nvidia/maxine/videofx), the os-release shim (OBC_TEST_PRELOAD) and override (MAXINE_OS_RELEASE) and an NVIDIA GPU"]
fn maxine_effects_run_from_the_default_root_with_nothing_set() {
    const NAME: &str = "maxine_effects_run_from_the_default_root_with_nothing_set";
    let default = Path::new(DEFAULT_ROOT);
    if std::env::var(CHILD_VAR).as_deref() != Ok(NAME) {
        assert!(
            default.is_dir(),
            "{DEFAULT_ROOT} is not a directory: this test needs the SDK the \
             maxine-vfx-sdk-local package installs there"
        );
        // No root at all: in_child removes MAXINE_VFX_ROOT and the
        // configured root, and nothing is added.
        assert!(in_child(NAME, &[]));
        return;
    }
    assert_eq!(std::env::var_os(ROOT_ENV), None);
    assert_eq!(obc_backend_maxine::configured_root(), None);
    assert_eq!(
        obc_backend_maxine::effective_root().as_deref(),
        Some(default)
    );
    effects_run_from(&MaxineFactory::from_env(), default);
}
