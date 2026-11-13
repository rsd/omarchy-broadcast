//! The committed C header is what cbindgen generates from `src/lib.rs`, and
//! a C compiler agrees with Rust on the layout it declares.
//!
//! `OBC_REGENERATE_HEADER=1 cargo test -p obc-module-abi --test header`
//! rewrites the header after a change to the ABI.

use std::mem::{offset_of, size_of};
use std::path::{Path, PathBuf};
use std::process::Command;

use obc_module_abi::*;

const HEADER: &str = "include/omarchy_broadcast_module.h";

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn generate() -> String {
    let dir = crate_dir();
    let config =
        cbindgen::Config::from_file(dir.join("cbindgen.toml")).expect("read cbindgen.toml");
    let bindings = cbindgen::Builder::new()
        .with_config(config)
        .with_src(dir.join("src/lib.rs"))
        .generate()
        .expect("cbindgen generates the header");
    let mut out = Vec::new();
    bindings.write(&mut out);
    strip_doc_links(&String::from_utf8(out).expect("the header is UTF-8"))
}

/// Turns the rustdoc links of the doc comments, ``[`name`]``, into plain
/// code spans, `` `name` ``, for C readers.
fn strip_doc_links(header: &str) -> String {
    let mut out = String::with_capacity(header.len());
    let mut rest = header;
    while let Some(start) = rest.find("[`") {
        let Some(len) = rest[start + 2..].find("`]") else {
            break;
        };
        out.push_str(&rest[..start]);
        out.push('`');
        out.push_str(&rest[start + 2..start + 2 + len]);
        out.push('`');
        rest = &rest[start + 2 + len + 2..];
    }
    out.push_str(rest);
    out
}

#[test]
fn committed_header_is_current() {
    let generated = generate();
    let path = crate_dir().join(HEADER);
    if std::env::var_os("OBC_REGENERATE_HEADER").is_some() {
        std::fs::write(&path, &generated).expect("write the header");
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == generated,
        "{HEADER} is out of date with src/lib.rs; regenerate it with \
         `OBC_REGENERATE_HEADER=1 cargo test -p obc-module-abi --test header` and commit it"
    );
}

/// `_Static_assert`s of the C layout against Rust's.
fn layout_checks() -> String {
    let mut checks = Vec::new();
    let mut size = |ty: &str, n: usize| {
        checks.push(format!(
            "_Static_assert(sizeof({ty}) == {n}, \"sizeof({ty})\");"
        ));
    };
    size("obc_host", size_of::<obc_host>());
    size("obc_plane", size_of::<obc_plane>());
    size("obc_cuda", size_of::<obc_cuda>());
    size("obc_frame", size_of::<obc_frame>());
    size("obc_value", size_of::<obc_value>());
    size("obc_param", size_of::<obc_param>());
    size("obc_chain_request", size_of::<obc_chain_request>());
    size("obc_load_report", size_of::<obc_load_report>());
    size("obc_timing", size_of::<obc_timing>());
    size("obc_status_value", size_of::<obc_status_value>());
    size("obc_audio_buffer", size_of::<obc_audio_buffer>());
    size("obc_audio_request", size_of::<obc_audio_request>());
    size("obc_audio_load_report", size_of::<obc_audio_load_report>());
    size("obc_module_vtable", size_of::<obc_module_vtable>());
    let mut offset = |ty: &str, field: &str, n: usize| {
        checks.push(format!(
            "_Static_assert(offsetof({ty}, {field}) == {n}, \"offsetof({ty}, {field})\");"
        ));
    };
    offset("obc_host", "log", offset_of!(obc_host, log));
    offset("obc_frame", "planes", offset_of!(obc_frame, planes));
    offset("obc_frame", "cuda", offset_of!(obc_frame, cuda));
    offset("obc_frame", "pts_ns", offset_of!(obc_frame, pts_ns));
    offset("obc_frame", "flags", offset_of!(obc_frame, flags));
    offset("obc_value", "data", offset_of!(obc_value, data));
    offset("obc_param", "value", offset_of!(obc_param, value));
    offset(
        "obc_chain_request",
        "full_range",
        offset_of!(obc_chain_request, full_range),
    );
    offset(
        "obc_chain_request",
        "cuda",
        offset_of!(obc_chain_request, cuda),
    );
    offset(
        "obc_chain_request",
        "background",
        offset_of!(obc_chain_request, background),
    );
    offset(
        "obc_load_report",
        "reused_model",
        offset_of!(obc_load_report, reused_model),
    );
    offset(
        "obc_load_report",
        "notice",
        offset_of!(obc_load_report, notice),
    );
    offset("obc_timing", "ms", offset_of!(obc_timing, ms));
    offset(
        "obc_status_value",
        "value",
        offset_of!(obc_status_value, value),
    );
    offset(
        "obc_status_value",
        "reserved0",
        offset_of!(obc_status_value, reserved0),
    );
    offset(
        "obc_status_value",
        "reserved1",
        offset_of!(obc_status_value, reserved1),
    );
    offset(
        "obc_module_vtable",
        "init",
        offset_of!(obc_module_vtable, init),
    );
    offset(
        "obc_module_vtable",
        "destroy",
        offset_of!(obc_module_vtable, destroy),
    );
    offset(
        "obc_module_vtable",
        "check",
        offset_of!(obc_module_vtable, check),
    );
    offset(
        "obc_module_vtable",
        "audio_instance_load",
        offset_of!(obc_module_vtable, audio_instance_load),
    );
    offset(
        "obc_module_vtable",
        "audio_instance_process",
        offset_of!(obc_module_vtable, audio_instance_process),
    );
    offset(
        "obc_module_vtable",
        "instance_status_values",
        offset_of!(obc_module_vtable, instance_status_values),
    );
    offset(
        "obc_module_vtable",
        "instance_set_background",
        offset_of!(obc_module_vtable, instance_set_background),
    );
    offset(
        "obc_audio_buffer",
        "planes",
        offset_of!(obc_audio_buffer, planes),
    );
    offset(
        "obc_audio_buffer",
        "pts_ns",
        offset_of!(obc_audio_buffer, pts_ns),
    );
    offset(
        "obc_audio_buffer",
        "voice_probability",
        offset_of!(obc_audio_buffer, voice_probability),
    );
    offset(
        "obc_audio_request",
        "sample_rate",
        offset_of!(obc_audio_request, sample_rate),
    );
    offset(
        "obc_audio_request",
        "params",
        offset_of!(obc_audio_request, params),
    );
    offset(
        "obc_audio_load_report",
        "latency_samples",
        offset_of!(obc_audio_load_report, latency_samples),
    );
    offset(
        "obc_audio_load_report",
        "reused_model",
        offset_of!(obc_audio_load_report, reused_model),
    );
    checks.push(format!(
        "_Static_assert(OBC_MODULE_ABI_VERSION == {OBC_MODULE_ABI_VERSION}u, \"version\");"
    ));
    checks.push(format!(
        "_Static_assert(OBC_MAX_AUDIO_CHANNELS == {OBC_MAX_AUDIO_CHANNELS}, \"channels\");"
    ));
    checks.push(format!(
        "_Static_assert(OBC_AUDIO_F32_PLANAR == {OBC_AUDIO_F32_PLANAR}, \"audio format\");"
    ));
    checks.push(format!(
        "_Static_assert(OBC_FORMAT_RGB_F32_PLANAR == {OBC_FORMAT_RGB_F32_PLANAR}, \"format\");"
    ));
    checks.join("\n")
}

/// Compiles `source` with `compiler`; `None` when the compiler is missing.
fn compile(compiler: &str, args: &[&str], source: &Path) -> Option<std::process::Output> {
    let include = crate_dir().join("include");
    Command::new(compiler)
        .args(args)
        .arg("-I")
        .arg(&include)
        .arg(source)
        .output()
        .ok()
}

#[test]
fn c_compiler_accepts_the_header_with_rust_layout() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let source = dir.join("obc_module_abi_layout.c");
    let program = format!(
        "#include <omarchy_broadcast_module.h>\n\
         #ifdef __cplusplus\n\
         #define _Static_assert static_assert\n\
         #endif\n\
         /* A module's exports, as a C module would define them. */\n\
         uint32_t obc_module_abi_version(void) {{ return OBC_MODULE_ABI_VERSION; }}\n\
         const obc_module_vtable *obc_module_entry(const obc_host *host) {{ (void)host; return NULL; }}\n\
         {}\n",
        layout_checks()
    );
    std::fs::write(&source, program).expect("write the C source");
    let flags = [
        "-std=c11",
        "-Wall",
        "-Wextra",
        "-Werror",
        "-pedantic",
        "-fsyntax-only",
    ];
    let Some(out) = compile("cc", &flags, &source) else {
        eprintln!("note: no `cc` on PATH; the header was not compiled");
        return;
    };
    assert!(
        out.status.success(),
        "cc rejected the header:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // C++ too, when there is a compiler for it (`cpp_compat`).
    let cpp_flags = [
        "-x",
        "c++",
        "-std=c++17",
        "-Wall",
        "-Wextra",
        "-Werror",
        "-fsyntax-only",
    ];
    match compile("c++", &cpp_flags, &source) {
        None => eprintln!("note: no `c++` on PATH; the header was not compiled as C++"),
        Some(out) => assert!(
            out.status.success(),
            "c++ rejected the header:\n{}",
            String::from_utf8_lossy(&out.stderr)
        ),
    }
}
