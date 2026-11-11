//! Building the module libraries and opening them through
//! `obc-module-host`, as the daemon does.

#![allow(dead_code)] // each test binary uses part of it

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use obc_module_host::{LogLevel, LogSink, Manifest, Module};

pub mod coexist;
pub mod gpu;

/// The crate's directory.
pub fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The packaged library line of a manifest.
pub fn packaged_library_line(lib: &str) -> String {
    format!("library = \"/usr/lib/omarchy-broadcast/modules/{lib}\"")
}

/// Builds the cdylib of `package` with cargo into a directory under
/// `CARGO_TARGET_TMPDIR` and returns the path of `lib` in it.
fn build(package: &str, lib: &str) -> PathBuf {
    let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("maxine-ar-modules");
    let status = Command::new(env!("CARGO"))
        .args([
            "build",
            "--quiet",
            "--locked",
            "-p",
            package,
            "--manifest-path",
        ])
        .arg(crate_dir().join("../../Cargo.toml"))
        .arg("--target-dir")
        .arg(&target)
        .status()
        .expect("run cargo");
    assert!(status.success(), "building {package} failed");
    let path = target.join("debug").join(lib);
    assert!(path.is_file(), "{} was not built", path.display());
    path
}

/// `libobc_module_maxine_ar.so`, built once per test binary.
pub fn module_library() -> PathBuf {
    static LIB: OnceLock<PathBuf> = OnceLock::new();
    LIB.get_or_init(|| build("obc-module-maxine-ar", "libobc_module_maxine_ar.so"))
        .clone()
}

/// The Video Effects module's `libobc_module_maxine.so`, built once per
/// test binary.
pub fn vfx_library() -> PathBuf {
    static LIB: OnceLock<PathBuf> = OnceLock::new();
    LIB.get_or_init(|| build("obc-module-maxine", "libobc_module_maxine.so"))
        .clone()
}

/// The manifest at `shipped`, with its library at `library`, as if
/// installed in `dir`.
pub fn install(dir: &Path, shipped: &Path, packaged_lib: &str, library: &Path) -> Manifest {
    let text = std::fs::read_to_string(shipped).unwrap();
    let installed = text.replace(
        &packaged_library_line(packaged_lib),
        &format!("library = \"{}\"", library.display()),
    );
    assert_ne!(text, installed, "the manifest's library line moved");
    let path = dir.join(shipped.file_name().unwrap());
    std::fs::write(&path, installed).unwrap();
    Manifest::load(&path).unwrap()
}

/// This crate's manifest, installed in `dir` with the built library.
pub fn installed_manifest(dir: &Path) -> Manifest {
    install(
        dir,
        &crate_dir().join("maxine-ar.toml"),
        "libobc_module_maxine_ar.so",
        &module_library(),
    )
}

/// The log lines of a module.
pub type Lines = Arc<Mutex<Vec<(LogLevel, String)>>>;

/// A log sink that keeps every line and prints it.
pub fn capture() -> (LogSink, Lines) {
    let lines: Lines = Arc::new(Mutex::new(Vec::new()));
    let keep = lines.clone();
    let sink: LogSink = Arc::new(move |id, level, msg| {
        println!("{id}: {level}: {msg}");
        keep.lock().unwrap().push((level, msg.to_string()));
    });
    (sink, lines)
}

/// Opens `manifest` with `config`.
pub fn open_manifest(manifest: &Manifest, config: serde_json::Value) -> Module {
    let (sink, _lines) = capture();
    Module::open(manifest, &config, sink)
        .unwrap_or_else(|e| panic!("the module {} does not open: {e}", manifest.id))
}

/// Opens this crate's built module with `config`.
pub fn open(dir: &Path, config: serde_json::Value) -> Module {
    open_manifest(&installed_manifest(dir), config)
}

/// Opens the Video Effects module (from `crates/obc-module-maxine`) with
/// `config`.
pub fn open_vfx(dir: &Path, config: serde_json::Value) -> Module {
    let manifest = install(
        dir,
        &crate_dir().join("../obc-module-maxine/maxine-vfx.toml"),
        "libobc_module_maxine.so",
        &vfx_library(),
    );
    open_manifest(&manifest, config)
}
