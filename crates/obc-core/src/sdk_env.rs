//! The NVIDIA Maxine SDK environment, applied by each daemon to itself at
//! start.
//!
//! A Maxine SDK package (built by each user from their own NVIDIA download
//! with the omarchy-nvidia-sdks repository) installs an environment file in
//! systemd `EnvironmentFile=` syntax and an os-release shim, a library the
//! process must preload: the SDK's model loader accepts only the
//! distributions NVIDIA lists, and the shim makes this process, and only
//! this one, read the file named by `MAXINE_OS_RELEASE` instead of
//! `/etc/os-release`. `LD_PRELOAD` (and `LD_LIBRARY_PATH`) only take effect
//! when a process starts, so a daemon that finds a complete SDK merges the
//! file into its environment, sets [`MARKER`] and executes itself again:
//! the path of its own executable (the one [`std::env::current_exe`]
//! names, so the process keeps its name; `/proc/self/exe` when that is
//! unknown), the same arguments (`argv[0]` included), the same PID
//! (an `exec`, not a new process), so a socket systemd handed over
//! (`LISTEN_FDS`, descriptor 3) is still this process's.
//!
//! The SDKs each daemon considers ([`Profile`]):
//!
//! - `omarchy-broadcastd` ([`BROADCASTD`]): the Video Effects SDK
//!   ([`VFX`]), then the AR SDK ([`AR`]), whose module runs in the same
//!   process and needs the same shim.
//! - `omarchy-broadcast-audiod` ([`AUDIOD`]): the Audio Effects SDK
//!   ([`AFX`]).
//!
//! An SDK is complete when both its environment file and its shim exist.
//! One that has neither is not installed, and is skipped silently (a debug
//! line). One that has only one of them, or whose environment file cannot
//! be read or parsed, is skipped with one warning that names the missing or
//! bad file (and the line, for a parse error); the daemon then runs without
//! that SDK, and the other SDK of its profile may still apply.
//!
//! The rules of the merge ([`decide`]):
//!
//! 1. A variable already set in the inherited environment is never
//!    overwritten: an explicit setting wins. `LD_PRELOAD` and
//!    `LD_LIBRARY_PATH` are merged instead, as below.
//! 2. `LD_PRELOAD`: the shim of the first complete SDK is appended to the
//!    inherited value (separator `:`), unless the value already names it
//!    (`:` and space both separate entries there). Exactly one shim is
//!    preloaded, and the `MAXINE_OS_RELEASE` applied is that SDK's; a later
//!    complete SDK contributes its other variables only.
//! 3. `LD_LIBRARY_PATH`, for the profiles that apply it (only the audio
//!    worker's: the Audio Effects SDK opens its feature libraries by name):
//!    when unset, the file's value; when set, the inherited entries first,
//!    then the file's entries not already present (empty entries skipped).
//!    `omarchy-broadcastd` never applies it: the video modules load the
//!    SDKs' libraries by absolute path, and the SDKs' bundled CUDA and
//!    TensorRT libraries on the search path would also be found by the
//!    other modules of the process. Every other dynamic loader variable of
//!    the file (`LD_*`, and `GCONV_PATH`) is never applied (a debug line).
//! 4. The SDK roots (`MAXINE_VFX_ROOT`, `MAXINE_AR_ROOT`,
//!    `MAXINE_AFX_ROOT`) are never applied: each module's default root is
//!    the package's, and a root variable would take precedence over a root
//!    set in the configuration. Every other variable of the file is applied
//!    when unset.
//! 5. Nothing is done when `OBC_NO_SDK_ENV` ([`OPT_OUT`]) is set to a
//!    non-empty value other than `0`; when [`MARKER`] is set (this process
//!    is the one executed again: it logs one line, `SDK environment applied
//!    from <files>`, and removes the marker, so the processes it starts
//!    decide for themselves); or when a shim of the profile, or any library
//!    named `os-release-shim.so` (an SDK tree installed without its
//!    package), is already mapped into the process (a service unit
//!    preloads it; the shim removes
//!    `LD_PRELOAD` from the environment when it loads, so the variable
//!    cannot tell).
//! 6. When executing again fails, the daemon logs it and runs without the
//!    SDK environment.
//!
//! [`decide`] is pure: it reads the inherited environment, the files and
//! whether a shim is mapped only through its arguments. [`apply`] is the
//! thin part that reads them from the process and executes again.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

/// Set in the environment of the process executed again: the environment
/// files applied, separated by `:`.
pub const MARKER: &str = "OBC_SDK_ENV_APPLIED";

/// Set to a non-empty value other than `0` to run without the SDK
/// environment (tests, or to run a daemon without the SDKs).
pub const OPT_OUT: &str = "OBC_NO_SDK_ENV";

/// The variables never applied (rule 4 of the module documentation).
const EXCLUDED: [&str; 3] = ["MAXINE_VFX_ROOT", "MAXINE_AR_ROOT", "MAXINE_AFX_ROOT"];

/// The file name of every os-release shim.
const SHIM_NAME: &str = "os-release-shim.so";

/// Whether `key` is a dynamic loader variable never applied from a file:
/// every `LD_*` but `LD_LIBRARY_PATH`, and `GCONV_PATH`.
fn loader_knob(key: &str) -> bool {
    (key.starts_with("LD_") && key != "LD_LIBRARY_PATH") || key == "GCONV_PATH"
}

/// One SDK package: its environment file and its os-release shim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sdk {
    /// The SDK's name, for the log.
    pub name: &'static str,
    /// The environment file the package installs.
    pub env_file: &'static str,
    /// The os-release shim the package installs.
    pub shim: &'static str,
}

/// The Video Effects SDK (`maxine-vfx-sdk-local`).
pub const VFX: Sdk = Sdk {
    name: "Maxine Video Effects SDK",
    env_file: "/usr/share/maxine-vfx/maxine-vfx.env",
    shim: "/usr/lib/maxine-vfx/os-release-shim.so",
};

/// The AR SDK (`maxine-ar-sdk-local`).
pub const AR: Sdk = Sdk {
    name: "Maxine AR SDK",
    env_file: "/usr/share/maxine-ar/maxine-ar.env",
    shim: "/usr/lib/maxine-ar/os-release-shim.so",
};

/// The Audio Effects SDK (`maxine-afx-sdk-local`).
pub const AFX: Sdk = Sdk {
    name: "Maxine Audio Effects SDK",
    env_file: "/usr/share/maxine-afx/maxine-afx.env",
    shim: "/usr/lib/maxine-afx/os-release-shim.so",
};

/// What one daemon applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    /// The SDKs, in order: the first complete one provides the shim.
    pub sdks: &'static [Sdk],
    /// Whether `LD_LIBRARY_PATH` is applied.
    pub library_path: bool,
}

/// `omarchy-broadcastd`: Video Effects, then AR; no `LD_LIBRARY_PATH`.
pub const BROADCASTD: Profile = Profile {
    sdks: &[VFX, AR],
    library_path: false,
};

/// `omarchy-broadcast-audiod`: Audio Effects, with `LD_LIBRARY_PATH`.
pub const AUDIOD: Profile = Profile {
    sdks: &[AFX],
    library_path: true,
};

/// The files [`decide`] reads.
pub trait Files {
    /// Whether a file exists at `path`.
    fn exists(&self, path: &str) -> bool;
    /// The file's text, or why it cannot be read.
    fn read(&self, path: &str) -> Result<String, String>;
}

/// A log line's level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Debug,
    Info,
    Warning,
}

/// One line for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub level: Level,
    pub text: String,
}

/// What the daemon does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Run as is: opted out, nothing to apply, or no complete SDK.
    Continue,
    /// This process was executed again with the environment of `files`:
    /// remove [`MARKER`] and run.
    Applied { files: Vec<String> },
    /// A shim is already mapped into the process: run as is.
    InPlace,
    /// Execute again with `set` added to the inherited environment
    /// ([`MARKER`] included).
    Reexec {
        set: Vec<(OsString, OsString)>,
        files: Vec<String>,
    },
}

/// [`decide`]'s result: the action and the lines to log first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub action: Action,
    pub notes: Vec<Note>,
}

/// A line of an environment file that cannot be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("line {line}: {reason}")]
pub struct ParseError {
    /// The line, from 1 (the first line of a continued one).
    pub line: usize,
    pub reason: &'static str,
}

/// Parses an environment file in systemd `EnvironmentFile=` syntax: blank
/// lines; comments, lines starting with `#` or `;`; `KEY=VALUE`, with
/// whitespace around the key and before the value ignored. A value is
/// unquoted (trailing whitespace dropped, a backslash escapes the next
/// character, a backslash at the end of the line continues it on the next
/// one), in single quotes (literal), or in double quotes (a backslash
/// escapes `"`, `\`, `$` and `` ` ``). A key set twice keeps its last
/// value. Lines may end in CRLF.
///
/// Where systemd's reading is easy to misjudge, the line is an error
/// instead: a quote that does not wrap the whole value (`A=foo"bar"`,
/// `A='x' y`), an unquoted value that ends in a backslash once trailing
/// whitespace is dropped (`A=x\ `), a continued line at the end of the
/// file, an `export ` prefix, and a quoted value spanning lines.
pub fn parse_env_file(text: &str) -> Result<Vec<(String, String)>, ParseError> {
    let mut vars: Vec<(String, String)> = Vec::new();
    let mut lines = text.lines().enumerate();
    while let Some((i, raw)) = lines.next() {
        let first = i + 1;
        let start = raw.trim_start();
        if start.is_empty() || start.starts_with('#') || start.starts_with(';') {
            continue;
        }
        let err = |reason| ParseError {
            line: first,
            reason,
        };
        let mut logical = start.to_string();
        while ends_with_escape(&logical) {
            logical.pop();
            match lines.next() {
                Some((_, next)) => logical.push_str(next),
                None => return Err(err("a continued line at the end of the file")),
            }
        }
        if logical
            .strip_prefix("export")
            .is_some_and(|rest| rest.starts_with([' ', '\t']))
        {
            return Err(err("an `export` prefix is not accepted"));
        }
        let Some((key, value)) = logical.split_once('=') else {
            return Err(err("expected KEY=VALUE"));
        };
        let key = key.trim();
        if !valid_name(key) {
            return Err(err("invalid variable name"));
        }
        let value = parse_value(value.trim_start()).map_err(err)?;
        vars.retain(|(k, _)| k != key);
        vars.push((key.to_string(), value));
    }
    Ok(vars)
}

/// Whether `s` ends with an odd number of backslashes.
fn ends_with_escape(s: &str) -> bool {
    s.bytes().rev().take_while(|&b| b == b'\\').count() % 2 == 1
}

fn valid_name(key: &str) -> bool {
    let mut bytes = key.bytes();
    matches!(bytes.next(), Some(b) if b == b'_' || b.is_ascii_alphabetic())
        && bytes.all(|b| b == b'_' || b.is_ascii_alphanumeric())
}

fn parse_value(v: &str) -> Result<String, &'static str> {
    let mut out = String::new();
    let mut chars = v.chars();
    match v.chars().next() {
        Some(q @ ('"' | '\'')) => {
            chars.next();
            loop {
                match chars.next() {
                    None => return Err("unterminated quote"),
                    Some(c) if c == q => break,
                    Some('\\') if q == '"' => match chars.next() {
                        Some(c @ ('"' | '\\' | '$' | '`')) => out.push(c),
                        Some(c) => {
                            out.push('\\');
                            out.push(c);
                        }
                        None => return Err("unterminated quote"),
                    },
                    Some(c) => out.push(c),
                }
            }
            if !chars.as_str().trim().is_empty() {
                return Err("a quote that does not wrap the whole value");
            }
        }
        _ => {
            let v = v.trim_end();
            if ends_with_escape(v) {
                return Err("a backslash at the end of the value");
            }
            let mut chars = v.chars();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        if let Some(n) = chars.next() {
                            out.push(n);
                        }
                    }
                    '"' | '\'' => return Err("a quote that does not wrap the whole value"),
                    c => out.push(c),
                }
            }
        }
    }
    Ok(out)
}

/// Whether `value` is set to opt out: non-empty and not `0`.
fn opted_out(value: Option<&OsStr>) -> bool {
    value.is_some_and(|v| !v.is_empty() && v != "0")
}

/// `LD_PRELOAD` with `shim` appended, or `None` when it already names it.
fn merge_preload(existing: Option<&OsStr>, shim: &str) -> Option<OsString> {
    let Some(existing) = existing.filter(|v| !v.is_empty()) else {
        return Some(shim.into());
    };
    let present = existing
        .as_bytes()
        .split(|&b| b == b':' || b == b' ')
        .any(|e| e == shim.as_bytes());
    if present {
        return None;
    }
    let mut v = existing.as_bytes().to_vec();
    v.push(b':');
    v.extend_from_slice(shim.as_bytes());
    Some(OsString::from_vec(v))
}

/// `LD_LIBRARY_PATH` with the entries of `file` appended that `existing`
/// lacks, or `None` when nothing is added.
fn merge_library_path(existing: Option<&OsStr>, file: &str) -> Option<OsString> {
    let mut v = existing.map(|e| e.as_bytes().to_vec()).unwrap_or_default();
    let mut added = false;
    for entry in file.split(':').filter(|e| !e.is_empty()) {
        if v.split(|&b| b == b':').any(|e| e == entry.as_bytes()) {
            continue;
        }
        if !v.is_empty() {
            v.push(b':');
        }
        v.extend_from_slice(entry.as_bytes());
        added = true;
    }
    added.then(|| OsString::from_vec(v))
}

fn lookup<'a>(env: &'a [(OsString, OsString)], key: &str) -> Option<&'a OsStr> {
    env.iter()
        .rev()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_os_str())
}

fn note(level: Level, text: String) -> Note {
    Note { level, text }
}

/// Decides what a daemon of `profile` does with the inherited environment
/// `env`, `shim_mapped` (whether a shim of the profile is mapped into the
/// process) and the files `files`, by the rules of the module
/// documentation.
pub fn decide(
    profile: &Profile,
    env: &[(OsString, OsString)],
    shim_mapped: bool,
    files: &dyn Files,
) -> Decision {
    let mut notes = Vec::new();
    let done = |action, notes| Decision { action, notes };
    if opted_out(lookup(env, OPT_OUT)) {
        notes.push(note(
            Level::Debug,
            format!("{OPT_OUT} is set: the SDK environment is not applied"),
        ));
        return done(Action::Continue, notes);
    }
    if let Some(marker) = lookup(env, MARKER) {
        let files: Vec<String> = marker
            .to_string_lossy()
            .split(':')
            .filter(|f| !f.is_empty())
            .map(String::from)
            .collect();
        notes.push(note(
            Level::Info,
            format!("SDK environment applied from {}", files.join(", ")),
        ));
        if !shim_mapped {
            notes.push(note(
                Level::Warning,
                "the os-release shim of the SDK environment is not loaded: the Maxine \
                 SDK may refuse to load its models"
                    .into(),
            ));
        }
        return done(Action::Applied { files }, notes);
    }
    if shim_mapped {
        notes.push(note(
            Level::Info,
            "SDK environment already in place (the os-release shim is loaded): not applied again"
                .into(),
        ));
        return done(Action::InPlace, notes);
    }

    let mut complete: Vec<(&Sdk, Vec<(String, String)>)> = Vec::new();
    for sdk in profile.sdks {
        let skip = |what: String| {
            note(
                Level::Warning,
                format!("{}: {what}; running without its environment", sdk.name),
            )
        };
        match (files.exists(sdk.env_file), files.exists(sdk.shim)) {
            (false, false) => notes.push(note(
                Level::Debug,
                format!("{} not installed ({} absent)", sdk.name, sdk.env_file),
            )),
            (true, false) => notes.push(skip(format!(
                "{} is missing (the environment file {} is present)",
                sdk.shim, sdk.env_file
            ))),
            (false, true) => notes.push(skip(format!(
                "{} is missing (the os-release shim {} is present)",
                sdk.env_file, sdk.shim
            ))),
            (true, true) => match files.read(sdk.env_file) {
                Err(e) => notes.push(skip(format!("cannot read {}: {e}", sdk.env_file))),
                Ok(text) => match parse_env_file(&text) {
                    Err(e) => notes.push(skip(format!("{}: {e}", sdk.env_file))),
                    Ok(vars) => complete.push((sdk, vars)),
                },
            },
        }
    }
    let Some(primary) = complete.first().map(|(sdk, _)| *sdk) else {
        return done(Action::Continue, notes);
    };

    let mut set: Vec<(OsString, OsString)> = Vec::new();
    if let Some(v) = merge_preload(lookup(env, "LD_PRELOAD"), primary.shim) {
        set.push(("LD_PRELOAD".into(), v));
    }
    for (i, (sdk, vars)) in complete.iter().enumerate() {
        for (key, value) in vars {
            let key = key.as_str();
            if loader_knob(key) {
                notes.push(note(
                    Level::Debug,
                    format!(
                        "{}: {key} is a dynamic loader variable: not applied",
                        sdk.env_file
                    ),
                ));
                continue;
            }
            if EXCLUDED.contains(&key)
                || key == MARKER
                || key == OPT_OUT
                || (key == "MAXINE_OS_RELEASE" && i != 0)
            {
                continue;
            }
            if key == "LD_LIBRARY_PATH" {
                if !profile.library_path {
                    continue;
                }
                let current = lookup(&set, key).or_else(|| lookup(env, key));
                if let Some(v) = merge_library_path(current, value) {
                    set.retain(|(k, _)| k != key);
                    set.push((key.into(), v));
                }
                continue;
            }
            if lookup(env, key).is_none() && lookup(&set, key).is_none() {
                set.push((key.into(), value.into()));
            }
        }
    }
    let applied: Vec<String> = complete
        .iter()
        .map(|(sdk, _)| sdk.env_file.to_string())
        .collect();
    if set.is_empty() {
        notes.push(note(
            Level::Debug,
            format!(
                "the environment already holds that of {}: not applied again",
                applied.join(", ")
            ),
        ));
        return done(Action::Continue, notes);
    }
    set.push((MARKER.into(), applied.join(":").into()));
    done(
        Action::Reexec {
            set,
            files: applied,
        },
        notes,
    )
}

/// The real files.
struct RealFiles;

impl Files for RealFiles {
    fn exists(&self, path: &str) -> bool {
        std::path::Path::new(path).is_file()
    }

    fn read(&self, path: &str) -> Result<String, String> {
        std::fs::read_to_string(path).map_err(|e| e.to_string())
    }
}

/// Whether `maps` (the text of `/proc/<pid>/maps`) maps one of `paths`, or
/// any file named [`SHIM_NAME`].
fn maps_have_shim(maps: &[u8], paths: &[Vec<u8>]) -> bool {
    let mut name = b"/".to_vec();
    name.extend_from_slice(SHIM_NAME.as_bytes());
    maps.split(|&b| b == b'\n')
        .any(|line| line.ends_with(&name) || paths.iter().any(|p| line.ends_with(p)))
}

/// Whether a shim of `profile` is mapped into this process (by its path,
/// the path it resolves to, or its file name).
fn shim_mapped(profile: &Profile) -> bool {
    let Ok(maps) = std::fs::read("/proc/self/maps") else {
        return false;
    };
    let mut paths: Vec<Vec<u8>> = Vec::new();
    for sdk in profile.sdks {
        paths.push(sdk.shim.as_bytes().to_vec());
        if let Ok(real) = std::fs::canonicalize(sdk.shim) {
            paths.push(real.into_os_string().into_vec());
        }
    }
    maps_have_shim(&maps, &paths)
}

/// The path to execute again: `current` (what [`std::env::current_exe`]
/// returned), so the kernel names the process after its executable, or
/// `/proc/self/exe` when it is unknown or the executable was replaced
/// (a path ending in ` (deleted)`). Executing `/proc/self/exe` itself
/// would name the process `exe`.
fn reexec_path(current: std::io::Result<std::path::PathBuf>) -> std::path::PathBuf {
    match current {
        Ok(p) if !p.as_os_str().as_bytes().ends_with(b" (deleted)") => p,
        _ => "/proc/self/exe".into(),
    }
}

/// Applies the SDK environment of `profile` to this process ([`decide`]
/// on the process's environment, files and mappings): logs the decision's
/// lines through `log`, executes the process again when it says so (and
/// returns only when that fails, after logging it), and removes [`MARKER`]
/// from the environment of the process executed again.
///
/// # Safety
///
/// Must be called before the process starts any thread: it may modify the
/// process environment ([`std::env::remove_var`]).
pub unsafe fn apply(profile: &Profile, log: &mut dyn FnMut(Level, &str)) {
    let env: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    let decision = decide(profile, &env, shim_mapped(profile), &RealFiles);
    for n in &decision.notes {
        log(n.level, &n.text);
    }
    match decision.action {
        Action::Continue | Action::InPlace => {}
        Action::Applied { .. } => {
            // SAFETY: the caller guarantees no other thread exists.
            unsafe { std::env::remove_var(MARKER) };
        }
        Action::Reexec { set, files } => {
            use std::os::unix::process::CommandExt;
            let mut args = std::env::args_os();
            let mut cmd = std::process::Command::new(reexec_path(std::env::current_exe()));
            if let Some(arg0) = args.next() {
                cmd.arg0(arg0);
            }
            let e = cmd.args(args).envs(set).exec();
            log(
                Level::Warning,
                &format!(
                    "cannot execute again with the SDK environment of {}: {e}; running without it",
                    files.join(", ")
                ),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// In-memory files: path → text.
    #[derive(Default)]
    struct Fake(BTreeMap<&'static str, Result<String, String>>);

    impl Fake {
        fn with(mut self, path: &'static str, text: &str) -> Self {
            self.0.insert(path, Ok(text.into()));
            self
        }
        fn unreadable(mut self, path: &'static str) -> Self {
            self.0.insert(path, Err("permission denied".into()));
            self
        }
        /// A complete SDK with `text` as its environment file.
        fn sdk(self, sdk: Sdk, text: &str) -> Self {
            self.with(sdk.env_file, text).with(sdk.shim, "")
        }
    }

    impl Files for Fake {
        fn exists(&self, path: &str) -> bool {
            self.0.contains_key(path)
        }
        fn read(&self, path: &str) -> Result<String, String> {
            self.0.get(path).cloned().unwrap_or(Err("not found".into()))
        }
    }

    const VFX_ENV: &str = "\
# NVIDIA Maxine Video Effects SDK
MAXINE_VFX_ROOT=/opt/nvidia/maxine/videofx
MAXINE_VFX_MODEL_DIR=/opt/nvidia/maxine/videofx/lib/models
LD_LIBRARY_PATH=/opt/nvidia/maxine/videofx/lib:/opt/nvidia/maxine/videofx/external/cuda/lib
MAXINE_VFX_OS_RELEASE_SHIM=/usr/lib/maxine-vfx/os-release-shim.so
MAXINE_OS_RELEASE=/usr/lib/maxine-vfx/fake-os-release
MAXINE_VFX_ARCHS=sm_120
";

    const AR_ENV: &str = "\
MAXINE_AR_ROOT=/opt/nvidia/maxine/arsdk
MAXINE_AR_MODEL_DIR=/opt/nvidia/maxine/arsdk/lib/models
LD_LIBRARY_PATH=/opt/nvidia/maxine/arsdk/lib
MAXINE_AR_OS_RELEASE_SHIM=/usr/lib/maxine-ar/os-release-shim.so
MAXINE_OS_RELEASE=/usr/lib/maxine-ar/fake-os-release
";

    const AFX_ENV: &str = "\
MAXINE_AFX_ROOT=/opt/nvidia/maxine/afx
LD_LIBRARY_PATH=/opt/nvidia/maxine/afx/nvafx/lib:/opt/nvidia/maxine/afx/features/denoiser/lib
MAXINE_OS_RELEASE=/usr/lib/maxine-afx/fake-os-release
";

    fn env(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    fn set_of(d: &Decision) -> BTreeMap<String, String> {
        match &d.action {
            Action::Reexec { set, .. } => set
                .iter()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
                .collect(),
            other => panic!("expected a re-exec, got {other:?}"),
        }
    }

    fn warnings(d: &Decision) -> Vec<&str> {
        d.notes
            .iter()
            .filter(|n| n.level == Level::Warning)
            .map(|n| n.text.as_str())
            .collect()
    }

    #[test]
    fn a_complete_vfx_sdk_is_applied_with_its_shim() {
        let files = Fake::default().sdk(VFX, VFX_ENV);
        let d = decide(&BROADCASTD, &env(&[("HOME", "/h")]), false, &files);
        let set = set_of(&d);
        assert_eq!(set["LD_PRELOAD"], VFX.shim);
        assert_eq!(
            set["MAXINE_OS_RELEASE"],
            "/usr/lib/maxine-vfx/fake-os-release"
        );
        assert_eq!(
            set["MAXINE_VFX_MODEL_DIR"],
            "/opt/nvidia/maxine/videofx/lib/models"
        );
        assert_eq!(set["MAXINE_VFX_ARCHS"], "sm_120");
        assert_eq!(set[MARKER], VFX.env_file);
        assert!(warnings(&d).is_empty(), "{:?}", d.notes);
        assert!(
            matches!(&d.action, Action::Reexec { files, .. } if files == &[VFX.env_file]),
            "{:?}",
            d.action
        );
    }

    #[test]
    fn broadcastd_never_gets_the_library_path_or_a_root() {
        let files = Fake::default().sdk(VFX, VFX_ENV).sdk(AR, AR_ENV);
        let set = set_of(&decide(&BROADCASTD, &[], false, &files));
        for key in ["LD_LIBRARY_PATH", "MAXINE_VFX_ROOT", "MAXINE_AR_ROOT"] {
            assert!(!set.contains_key(key), "{key} applied: {set:?}");
        }
    }

    #[test]
    fn audiod_never_gets_its_root() {
        let files = Fake::default().sdk(AFX, AFX_ENV);
        let set = set_of(&decide(&AUDIOD, &[], false, &files));
        assert!(!set.contains_key("MAXINE_AFX_ROOT"));
        assert_eq!(set["LD_PRELOAD"], AFX.shim);
        assert_eq!(
            set["MAXINE_OS_RELEASE"],
            "/usr/lib/maxine-afx/fake-os-release"
        );
    }

    #[test]
    fn an_absent_sdk_runs_without_it_silently() {
        let d = decide(&BROADCASTD, &[], false, &Fake::default());
        assert_eq!(d.action, Action::Continue);
        assert!(d.notes.iter().all(|n| n.level == Level::Debug), "{d:?}");
    }

    #[test]
    fn an_env_file_without_its_shim_is_one_warning_naming_the_shim() {
        let files = Fake::default().with(AFX.env_file, AFX_ENV);
        let d = decide(&AUDIOD, &[], false, &files);
        assert_eq!(d.action, Action::Continue);
        let w = warnings(&d);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(
            w[0].contains(&format!("{} is missing", AFX.shim)),
            "{}",
            w[0]
        );
    }

    #[test]
    fn a_shim_without_its_env_file_is_one_warning_naming_the_file() {
        let files = Fake::default().with(AFX.shim, "");
        let d = decide(&AUDIOD, &[], false, &files);
        assert_eq!(d.action, Action::Continue);
        let w = warnings(&d);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(
            w[0].contains(&format!("{} is missing", AFX.env_file)),
            "{}",
            w[0]
        );
    }

    #[test]
    fn an_unparsable_env_file_names_the_file_and_line() {
        let files = Fake::default().sdk(AFX, "# ok\nMAXINE_X=1\nnot a variable\n");
        let d = decide(&AUDIOD, &[], false, &files);
        assert_eq!(d.action, Action::Continue);
        let w = warnings(&d);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(
            w[0].contains(&format!("{}: line 3:", AFX.env_file)),
            "{}",
            w[0]
        );
    }

    #[test]
    fn an_unreadable_env_file_is_named() {
        let files = Fake::default().unreadable(AFX.env_file).with(AFX.shim, "");
        let d = decide(&AUDIOD, &[], false, &files);
        assert_eq!(d.action, Action::Continue);
        let w = warnings(&d);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains(&format!("cannot read {}", AFX.env_file)));
    }

    #[test]
    fn the_marker_means_applied_and_logs_the_files() {
        let files = Fake::default().sdk(VFX, VFX_ENV).sdk(AR, AR_ENV);
        let marker = format!("{}:{}", VFX.env_file, AR.env_file);
        let d = decide(&BROADCASTD, &env(&[(MARKER, &marker)]), true, &files);
        assert_eq!(
            d.action,
            Action::Applied {
                files: vec![VFX.env_file.into(), AR.env_file.into()]
            }
        );
        assert_eq!(d.notes.len(), 1, "{:?}", d.notes);
        assert_eq!(d.notes[0].level, Level::Info);
        assert_eq!(
            d.notes[0].text,
            format!(
                "SDK environment applied from {}, {}",
                VFX.env_file, AR.env_file
            )
        );
    }

    #[test]
    fn the_marker_without_the_shim_loaded_warns() {
        let d = decide(
            &AUDIOD,
            &env(&[(MARKER, AFX.env_file)]),
            false,
            &Fake::default(),
        );
        assert!(matches!(d.action, Action::Applied { .. }));
        assert_eq!(warnings(&d).len(), 1);
    }

    #[test]
    fn a_shim_already_mapped_is_not_applied_again() {
        let files = Fake::default().sdk(VFX, VFX_ENV);
        let d = decide(&BROADCASTD, &[], true, &files);
        assert_eq!(d.action, Action::InPlace);
        assert!(warnings(&d).is_empty());
    }

    #[test]
    fn the_opt_out_skips_everything() {
        let files = Fake::default().sdk(VFX, VFX_ENV).with(AR.shim, "");
        for value in ["1", "yes"] {
            let d = decide(&BROADCASTD, &env(&[(OPT_OUT, value)]), false, &files);
            assert_eq!(d.action, Action::Continue);
            assert!(warnings(&d).is_empty());
        }
        for value in ["", "0"] {
            let d = decide(&BROADCASTD, &env(&[(OPT_OUT, value)]), false, &files);
            assert!(matches!(d.action, Action::Reexec { .. }), "{value:?}");
        }
    }

    #[test]
    fn ld_preload_is_appended_once() {
        let files = Fake::default().sdk(VFX, VFX_ENV);
        let unset = set_of(&decide(&BROADCASTD, &[], false, &files));
        assert_eq!(unset["LD_PRELOAD"], VFX.shim);
        let other = set_of(&decide(
            &BROADCASTD,
            &env(&[("LD_PRELOAD", "/usr/lib/libother.so")]),
            false,
            &files,
        ));
        assert_eq!(
            other["LD_PRELOAD"],
            format!("/usr/lib/libother.so:{}", VFX.shim)
        );
        for existing in [
            VFX.shim.to_string(),
            format!("/usr/lib/libother.so {}", VFX.shim),
            format!("{}:/usr/lib/libother.so", VFX.shim),
        ] {
            let d = decide(
                &BROADCASTD,
                &env(&[("LD_PRELOAD", &existing)]),
                false,
                &files,
            );
            assert!(!set_of(&d).contains_key("LD_PRELOAD"), "{existing}");
        }
    }

    #[test]
    fn ld_library_path_is_merged_for_audiod() {
        let files = Fake::default().sdk(AFX, AFX_ENV);
        let unset = set_of(&decide(&AUDIOD, &[], false, &files));
        assert_eq!(
            unset["LD_LIBRARY_PATH"],
            "/opt/nvidia/maxine/afx/nvafx/lib:/opt/nvidia/maxine/afx/features/denoiser/lib"
        );
        let existing = set_of(&decide(
            &AUDIOD,
            &env(&[("LD_LIBRARY_PATH", "/usr/local/lib")]),
            false,
            &files,
        ));
        assert_eq!(
            existing["LD_LIBRARY_PATH"],
            "/usr/local/lib:/opt/nvidia/maxine/afx/nvafx/lib:/opt/nvidia/maxine/afx/features/denoiser/lib"
        );
        let dedupe = set_of(&decide(
            &AUDIOD,
            &env(&[(
                "LD_LIBRARY_PATH",
                "/opt/nvidia/maxine/afx/features/denoiser/lib:/x",
            )]),
            false,
            &files,
        ));
        assert_eq!(
            dedupe["LD_LIBRARY_PATH"],
            "/opt/nvidia/maxine/afx/features/denoiser/lib:/x:/opt/nvidia/maxine/afx/nvafx/lib"
        );
        let all = "/opt/nvidia/maxine/afx/nvafx/lib:/opt/nvidia/maxine/afx/features/denoiser/lib";
        let none = set_of(&decide(
            &AUDIOD,
            &env(&[("LD_LIBRARY_PATH", all)]),
            false,
            &files,
        ));
        assert!(!none.contains_key("LD_LIBRARY_PATH"));
    }

    #[test]
    fn empty_library_path_entries_are_skipped() {
        let files = Fake::default().sdk(AFX, "LD_LIBRARY_PATH=/a::/b:\n");
        let set = set_of(&decide(&AUDIOD, &[], false, &files));
        assert_eq!(set["LD_LIBRARY_PATH"], "/a:/b");
    }

    #[test]
    fn a_user_variable_is_not_overwritten() {
        let files = Fake::default().sdk(VFX, VFX_ENV);
        let set = set_of(&decide(
            &BROADCASTD,
            &env(&[
                ("MAXINE_OS_RELEASE", "/home/u/os-release"),
                ("MAXINE_VFX_ARCHS", "mine"),
            ]),
            false,
            &files,
        ));
        assert!(!set.contains_key("MAXINE_OS_RELEASE"));
        assert!(!set.contains_key("MAXINE_VFX_ARCHS"));
        assert!(set.contains_key("MAXINE_VFX_MODEL_DIR"));
    }

    #[test]
    fn vfx_and_ar_complete_preload_one_shim_with_vfx_os_release() {
        let files = Fake::default().sdk(VFX, VFX_ENV).sdk(AR, AR_ENV);
        let d = decide(&BROADCASTD, &[], false, &files);
        let set = set_of(&d);
        assert_eq!(set["LD_PRELOAD"], VFX.shim);
        assert_eq!(
            set["MAXINE_OS_RELEASE"],
            "/usr/lib/maxine-vfx/fake-os-release"
        );
        assert_eq!(
            set["MAXINE_AR_MODEL_DIR"],
            "/opt/nvidia/maxine/arsdk/lib/models"
        );
        assert_eq!(set[MARKER], format!("{}:{}", VFX.env_file, AR.env_file));
    }

    #[test]
    fn a_half_installed_vfx_leaves_ar_to_apply() {
        let files = Fake::default().with(VFX.env_file, VFX_ENV).sdk(AR, AR_ENV);
        let d = decide(&BROADCASTD, &[], false, &files);
        assert_eq!(warnings(&d).len(), 1);
        let set = set_of(&d);
        assert_eq!(set["LD_PRELOAD"], AR.shim);
        assert_eq!(
            set["MAXINE_OS_RELEASE"],
            "/usr/lib/maxine-ar/fake-os-release"
        );
        assert!(!set.contains_key("MAXINE_VFX_MODEL_DIR"));
        assert_eq!(set[MARKER], AR.env_file);
    }

    #[test]
    fn nothing_to_set_means_no_reexec() {
        let files = Fake::default().sdk(AFX, "MAXINE_OS_RELEASE=/x\n");
        let d = decide(
            &AUDIOD,
            &env(&[("MAXINE_OS_RELEASE", "/y"), ("LD_PRELOAD", AFX.shim)]),
            false,
            &files,
        );
        assert_eq!(d.action, Action::Continue);
    }

    #[test]
    fn the_parser_reads_comments_quotes_and_blank_lines() {
        let text = "\
# a comment
  ; another

A=1
 B = two words
C=\"quoted # not a comment\"
D='single \\n literal'
E=\"esc \\\" \\\\ \\$ \\x\"
F=
G=continued\\
 line
A=last
";
        let vars = parse_env_file(text).unwrap();
        let get = |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("A"), Some("last"));
        assert_eq!(get("B"), Some("two words"));
        assert_eq!(get("C"), Some("quoted # not a comment"));
        assert_eq!(get("D"), Some("single \\n literal"));
        assert_eq!(get("E"), Some("esc \" \\ $ \\x"));
        assert_eq!(get("F"), Some(""));
        assert_eq!(get("G"), Some("continued line"));
        assert_eq!(vars.len(), 7);
    }

    #[test]
    fn the_parser_reports_the_bad_line() {
        let cases = [
            ("A=1\nnothing here\n", 2, "expected KEY=VALUE"),
            ("\n\n1A=x\n", 3, "invalid variable name"),
            ("A=\"open\n", 1, "unterminated quote"),
            ("A='x' y\n", 1, "a quote that does not wrap the whole value"),
            (
                "A=foo\"bar\"\n",
                1,
                "a quote that does not wrap the whole value",
            ),
            ("A=it's\n", 1, "a quote that does not wrap the whole value"),
            ("A=x\\ \n", 1, "a backslash at the end of the value"),
            ("A=x\\\\\\ \n", 1, "a backslash at the end of the value"),
            ("B=1\nA=x\\", 2, "a continued line at the end of the file"),
            ("export A=1\n", 1, "an `export` prefix is not accepted"),
            ("\texport\tA=1\n", 1, "an `export` prefix is not accepted"),
        ];
        for (text, line, reason) in cases {
            assert_eq!(
                parse_env_file(text),
                Err(ParseError { line, reason }),
                "{text:?}"
            );
        }
    }

    #[test]
    fn the_parser_tolerates_crlf() {
        let vars = parse_env_file("# c\r\nA=1\r\nB=\"two\"\r\nC=x\\\r\n y\r\n").unwrap();
        assert_eq!(
            vars,
            [
                ("A".to_string(), "1".to_string()),
                ("B".into(), "two".into()),
                ("C".into(), "x y".into()),
            ]
        );
    }

    #[test]
    fn the_parser_accepts_escaped_quotes_and_backslashes() {
        let vars = parse_env_file("A=a\\\"b\nB=x\\\\\nC=a\\ b\n").unwrap();
        let get = |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("A"), Some("a\"b"));
        assert_eq!(get("B"), Some("x\\"));
        assert_eq!(get("C"), Some("a b"));
    }

    #[test]
    fn a_file_with_a_parse_error_is_reported_once_and_not_applied() {
        let files = Fake::default().sdk(AFX, "MAXINE_X=1\nexport MAXINE_Y=2\n");
        let d = decide(&AUDIOD, &[], false, &files);
        assert_eq!(d.action, Action::Continue);
        let w = warnings(&d);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("line 2: an `export` prefix"), "{}", w[0]);
    }

    #[test]
    fn loader_variables_other_than_the_library_path_are_refused() {
        let text = "\
LD_PRELOAD=/evil.so
LD_AUDIT=/audit.so
LD_BIND_NOW=1
GCONV_PATH=/gconv
LD_LIBRARY_PATH=/opt/afx/lib
MAXINE_X=1
";
        let files = Fake::default().sdk(AFX, text);
        let d = decide(&AUDIOD, &[], false, &files);
        let set = set_of(&d);
        assert_eq!(set["LD_PRELOAD"], AFX.shim);
        assert_eq!(set["LD_LIBRARY_PATH"], "/opt/afx/lib");
        assert_eq!(set["MAXINE_X"], "1");
        for key in ["LD_AUDIT", "LD_BIND_NOW", "GCONV_PATH"] {
            assert!(!set.contains_key(key), "{key} applied: {set:?}");
        }
        for key in ["LD_PRELOAD", "LD_AUDIT", "LD_BIND_NOW", "GCONV_PATH"] {
            assert!(
                d.notes
                    .iter()
                    .any(|n| n.level == Level::Debug && n.text.contains(key)),
                "{key}: {:?}",
                d.notes
            );
        }
        assert!(warnings(&d).is_empty(), "{:?}", d.notes);
    }

    #[test]
    fn any_mapped_os_release_shim_counts_as_in_place() {
        let paths = [AFX.shim.as_bytes().to_vec()];
        let line = |path: &str| {
            format!("7f00-7f01 r-xp 00000000 103:02 4242                       {path}\n")
        };
        let maps = |p: &str| format!("{}{}", line("/usr/lib/libc.so.6"), line(p));
        assert!(maps_have_shim(maps(AFX.shim).as_bytes(), &paths));
        assert!(maps_have_shim(
            maps("/home/u/.local/lib/maxine-afx/os-release-shim.so").as_bytes(),
            &paths
        ));
        assert!(!maps_have_shim(
            maps("/usr/lib/libm.so.6").as_bytes(),
            &paths
        ));
        assert!(!maps_have_shim(
            maps("/usr/lib/not-os-release-shim.so").as_bytes(),
            &paths
        ));
    }

    #[test]
    fn the_reexec_path_keeps_the_executable_name() {
        use std::path::PathBuf;
        let exe = PathBuf::from("/usr/bin/omarchy-broadcastd");
        assert_eq!(reexec_path(Ok(exe.clone())), exe);
        assert_eq!(
            reexec_path(Ok("/usr/bin/omarchy-broadcastd (deleted)".into())),
            PathBuf::from("/proc/self/exe")
        );
        assert_eq!(
            reexec_path(Err(std::io::Error::other("no /proc"))),
            PathBuf::from("/proc/self/exe")
        );
    }

    /// The environment files of the SDK packages installed on this machine
    /// (none in CI): each parses and holds the keys the daemons rely on.
    #[test]
    fn the_installed_packages_files_parse() {
        for (sdk, upper) in [(VFX, "VFX"), (AR, "AR"), (AFX, "AFX")] {
            let dir = std::path::Path::new(sdk.env_file).parent().unwrap();
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries {
                let path = entry.unwrap().path();
                if path.extension().is_none_or(|e| e != "env") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                let vars = parse_env_file(&text).unwrap_or_else(|e| panic!("{path:?}: {e}"));
                let keys: Vec<&str> = vars.iter().map(|(k, _)| k.as_str()).collect();
                for key in [
                    format!("MAXINE_{upper}_ROOT"),
                    format!("MAXINE_{upper}_OS_RELEASE_SHIM"),
                    "MAXINE_OS_RELEASE".into(),
                    "LD_LIBRARY_PATH".into(),
                ] {
                    assert!(
                        keys.contains(&key.as_str()),
                        "{path:?} lacks {key}: {keys:?}"
                    );
                }
                let shim = vars.iter().find(|(k, _)| k.ends_with("_OS_RELEASE_SHIM"));
                assert_eq!(shim.map(|(_, v)| v.as_str()), Some(sdk.shim), "{path:?}");
                assert!(!keys.iter().any(|k| loader_knob(k)), "{path:?}: {keys:?}");
            }
        }
    }

    #[test]
    fn the_packages_files_parse() {
        for text in [VFX_ENV, AR_ENV, AFX_ENV] {
            parse_env_file(text).unwrap();
        }
    }
}
