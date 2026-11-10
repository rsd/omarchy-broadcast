//! Building the module library and opening it through `obc-module-host`, as
//! the daemon does.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use obc_module_host::{LogLevel, LogSink, Manifest, Module};

/// The crate's directory.
pub fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Builds `libobc_module_maxine.so` with cargo into a directory under
/// `CARGO_TARGET_TMPDIR` (once per test binary) and returns its path.
pub fn module_library() -> PathBuf {
    static LIB: OnceLock<PathBuf> = OnceLock::new();
    LIB.get_or_init(|| {
        let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("maxine-module");
        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "--quiet",
                "--locked",
                "-p",
                "obc-module-maxine",
                "--manifest-path",
            ])
            .arg(crate_dir().join("../../Cargo.toml"))
            .arg("--target-dir")
            .arg(&target)
            .status()
            .expect("run cargo");
        assert!(status.success(), "building the module failed");
        let lib = target.join("debug/libobc_module_maxine.so");
        assert!(lib.is_file(), "{} was not built", lib.display());
        lib
    })
    .clone()
}

/// The shipped manifest, with its library at `library`, as if installed in
/// `dir`.
pub fn installed_manifest(dir: &Path, library: &Path) -> Manifest {
    let shipped = std::fs::read_to_string(crate_dir().join("maxine-vfx.toml")).unwrap();
    let installed = shipped.replace(
        "library = \"/usr/lib/omarchy-broadcast/modules/libobc_module_maxine.so\"",
        &format!("library = \"{}\"", library.display()),
    );
    assert_ne!(shipped, installed, "the manifest's library line moved");
    let path = dir.join("maxine-vfx.toml");
    std::fs::write(&path, installed).unwrap();
    Manifest::load(&path).unwrap()
}

/// The log lines of the module.
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

/// Opens the built module with `config`.
pub fn open(dir: &Path, config: serde_json::Value) -> (Module, Lines) {
    let manifest = installed_manifest(dir, &module_library());
    let (sink, lines) = capture();
    let module = Module::open(&manifest, &config, sink)
        .unwrap_or_else(|e| panic!("the module does not open: {e}"));
    (module, lines)
}
