//! The audio settings snapshot: everything the worker needs to know of the
//! user's microphone choices, pushed by the video daemon (the single
//! writer of every setting, ADR 0026) with `ApplySnapshot` and written by
//! it atomically to `$XDG_STATE_HOME/omarchy-broadcast/audio-settings.json`.
//!
//! The worker reads the file at start, so it runs correctly with the
//! daemon down; it never writes it. A missing file, one larger than
//! [`MAX_FILE_SIZE`], one that is not a JSON object or one of another
//! version gives the defaults, with a note saying why; in a file that is
//! otherwise good, a field that does not fit takes its default (with a
//! note) and the others are kept, so one bad value never loses the user's
//! disabled modules. Nothing here is ever fatal. `ApplySnapshot` is strict
//! ([`Snapshot::parse`]): the daemon hears what is wrong.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use obc_module_host::Profile;
use serde::{Deserialize, Serialize};

/// The snapshot's file name, in the state directory.
pub const FILE: &str = "audio-settings.json";

/// The crash ledger's file name, beside the snapshot.
pub const LEDGER_FILE: &str = "audio-modules-crash.json";

/// The only snapshot version.
pub const VERSION: u32 = 1;

/// The default `idle_exit_s`.
pub const DEFAULT_IDLE_EXIT_S: u64 = 30;

/// The default `gpu_warm_s`.
pub const DEFAULT_GPU_WARM_S: u64 = 30;

/// The largest snapshot file read: 1 MiB.
pub const MAX_FILE_SIZE: u64 = 1 << 20;

/// The longest `idle_exit_s` taken: a day.
pub const MAX_IDLE_EXIT_S: u64 = 86_400;

/// The longest `gpu_warm_s` taken: a day, as `idle_exit_s`.
pub const MAX_GPU_WARM_S: u64 = MAX_IDLE_EXIT_S;

/// The pipeline depth: by device, or one value for every chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PipelineDepth {
    /// The same depth whatever the chain runs on.
    Override(u32),
    /// The depth of a chain that runs on the CPU only, and of one with a
    /// stage on a GPU (or an NPU).
    ByDevice {
        #[serde(default = "one")]
        cpu: u32,
        #[serde(default = "two")]
        gpu: u32,
    },
}

fn one() -> u32 {
    1
}

fn two() -> u32 {
    2
}

impl Default for PipelineDepth {
    fn default() -> Self {
        PipelineDepth::ByDevice { cpu: 1, gpu: 2 }
    }
}

impl PipelineDepth {
    /// The depth of a chain, `gpu` when any of its stages runs off the CPU,
    /// within 1..=[`obc_audio::MAX_PIPELINE_DEPTH`].
    pub fn blocks(self, gpu: bool) -> u32 {
        let d = match self {
            PipelineDepth::Override(d) => d,
            PipelineDepth::ByDevice { cpu, gpu: g } => {
                if gpu {
                    g
                } else {
                    cpu
                }
            }
        };
        d.clamp(1, obc_audio::MAX_PIPELINE_DEPTH)
    }
}

/// The snapshot (see the module documentation). Unknown keys are ignored;
/// a key left out takes its default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    /// The format version, [`VERSION`].
    pub version: u32,
    /// Processing on; off is raw passthrough, never silence.
    pub enabled: bool,
    /// The capture device: `default` or a `node.name` (PipeWire's business).
    pub device: String,
    /// The pipeline depth.
    pub pipeline_depth: PipelineDepth,
    /// Seconds without an input block (and without a D-Bus call) before
    /// the worker exits; 0 never.
    pub idle_exit_s: u64,
    /// Seconds the GPU stages stay planned after the last application
    /// stops recording (the worker stays meanwhile); 0 drops them at once.
    pub gpu_warm_s: u64,
    /// `always`, `with-camera` or `never` (see [`GpuPolicy`]).
    pub gpu_policy: String,
    /// The camera session is active (the daemon says so); read for
    /// `gpu_policy = "with-camera"` when a mic session starts.
    pub camera_active: bool,
    /// The backend profile, `quality` or `battery`.
    pub profile: String,
    /// The module each pinned stage must run on.
    pub pins: BTreeMap<String, String>,
    /// The values of the modules' `mic.*` settings.
    pub values: BTreeMap<String, serde_json::Value>,
    /// The modules the user disabled.
    pub disabled_modules: Vec<String>,
    /// Run stages together that declare a conflict (for testing).
    pub forced_conflicts: bool,
}

impl Default for Snapshot {
    fn default() -> Self {
        Snapshot {
            version: VERSION,
            enabled: true,
            device: "default".into(),
            pipeline_depth: PipelineDepth::default(),
            idle_exit_s: DEFAULT_IDLE_EXIT_S,
            gpu_warm_s: DEFAULT_GPU_WARM_S,
            gpu_policy: GpuPolicy::Always.as_str().into(),
            camera_active: false,
            profile: Profile::Quality.as_str().into(),
            pins: BTreeMap::new(),
            values: BTreeMap::new(),
            disabled_modules: Vec::new(),
            forced_conflicts: false,
        }
    }
}

impl Snapshot {
    /// Parses a snapshot. The error says why it cannot be used: it is not
    /// JSON of this shape, or of another version.
    pub fn parse(text: &str) -> Result<Snapshot, String> {
        let s: Snapshot = serde_json::from_str(text).map_err(|e| e.to_string())?;
        if s.version != VERSION {
            return Err(format!(
                "it is of version {}; this worker reads version {VERSION}",
                s.version
            ));
        }
        Ok(s.normalised())
    }

    /// Parses a snapshot field by field: the text must be a JSON object of
    /// this version; a field that does not fit takes its default, with a
    /// note naming it. Unknown keys are ignored.
    pub fn parse_lenient(text: &str) -> Result<(Snapshot, Vec<String>), String> {
        let value: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let serde_json::Value::Object(fields) = value else {
            return Err("it is not a JSON object".into());
        };
        match fields.get("version") {
            None => {}
            Some(v) if v.as_u64() == Some(u64::from(VERSION)) => {}
            Some(v) => {
                return Err(format!(
                    "it is of version {v}; this worker reads version {VERSION}"
                ));
            }
        }
        let defaults = serde_json::to_value(Snapshot::default()).map_err(|e| e.to_string())?;
        let serde_json::Value::Object(mut merged) = defaults else {
            return Err("the defaults are not an object".into());
        };
        let mut notes = Vec::new();
        for (key, v) in fields {
            let Some(default) = merged.get(&key).cloned() else {
                continue;
            };
            // Each field alone: does it fit its type?
            let mut probe = merged.clone();
            probe.insert(key.clone(), v.clone());
            match serde_json::from_value::<Snapshot>(serde_json::Value::Object(probe)) {
                Ok(_) => {
                    merged.insert(key, v);
                }
                Err(e) => {
                    notes.push(format!("{key}: {e}; using its default"));
                    merged.insert(key, default);
                }
            }
        }
        let s: Snapshot =
            serde_json::from_value(serde_json::Value::Object(merged)).map_err(|e| e.to_string())?;
        Ok((s.normalised(), notes))
    }

    /// The snapshot in `path`, else the defaults with a note saying why
    /// (missing, unreadable, too large, not valid); see the module
    /// documentation.
    pub fn load(path: &Path) -> (Snapshot, Option<String>) {
        match read_capped(path) {
            Ok(text) => match Snapshot::parse_lenient(&text) {
                Ok((s, notes)) if notes.is_empty() => (s, None),
                Ok((s, notes)) => (
                    s,
                    Some(format!(
                        "the audio settings {} have values that cannot be used ({})",
                        path.display(),
                        notes.join("; ")
                    )),
                ),
                Err(e) => (
                    Snapshot::default(),
                    Some(format!(
                        "the audio settings {} cannot be used ({e}): using the defaults",
                        path.display()
                    )),
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (
                Snapshot::default(),
                Some(format!(
                    "no audio settings at {}: using the defaults",
                    path.display()
                )),
            ),
            Err(e) => (
                Snapshot::default(),
                Some(format!(
                    "the audio settings {} cannot be read ({e}): using the defaults",
                    path.display()
                )),
            ),
        }
    }

    /// The snapshot with its numbers in range.
    fn normalised(mut self) -> Snapshot {
        self.idle_exit_s = self.idle_exit_s.min(MAX_IDLE_EXIT_S);
        self.gpu_warm_s = self.gpu_warm_s.min(MAX_GPU_WARM_S);
        self
    }

    /// The GPU policy (`always` when it names none this worker knows).
    pub fn gpu_policy(&self) -> GpuPolicy {
        GpuPolicy::parse(&self.gpu_policy)
    }

    /// The backend profile (`quality` when it names none this worker
    /// knows).
    pub fn profile(&self) -> Profile {
        self.profile.parse().unwrap_or(Profile::Quality)
    }
}

/// The text of `path`, refused beyond [`MAX_FILE_SIZE`].
fn read_capped(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    let mut text = String::new();
    file.take(MAX_FILE_SIZE + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX_FILE_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("larger than {MAX_FILE_SIZE} bytes"),
        ));
    }
    Ok(text)
}

/// When the microphone's stages may run on a GPU (`gpu` or `nvidia-gpu`;
/// an NPU and the CPU always may).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuPolicy {
    /// Whenever a GPU module ranks first.
    Always,
    /// Only in a mic session that started while the camera session was
    /// active (decided at the start, never switched mid-session).
    WithCamera,
    /// Never: GPU providers are skipped.
    Never,
}

impl GpuPolicy {
    /// The snapshot's name.
    pub fn as_str(self) -> &'static str {
        match self {
            GpuPolicy::Always => "always",
            GpuPolicy::WithCamera => "with-camera",
            GpuPolicy::Never => "never",
        }
    }

    /// The policy named `s`; `always` for any other text.
    pub fn parse(s: &str) -> GpuPolicy {
        match s {
            "with-camera" => GpuPolicy::WithCamera,
            "never" => GpuPolicy::Never,
            _ => GpuPolicy::Always,
        }
    }
}

/// The snapshot's path: `$XDG_STATE_HOME/omarchy-broadcast/audio-settings.json`,
/// else `~/.local/state/omarchy-broadcast/audio-settings.json`.
pub fn default_path() -> Option<PathBuf> {
    obc_core::config::state_path().and_then(|p| p.parent().map(|d| d.join(FILE)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_snapshot_parses() {
        let s = Snapshot::parse(
            r#"{"version":1,"enabled":false,"device":"alsa_input.x","pipeline_depth":3,
                "idle_exit_s":5,"gpu_warm_s":7,"gpu_policy":"never","profile":"battery",
                "pins":{"mic-denoise":"m"},"values":{"mic.a.b":true},
                "disabled_modules":["x"],"forced_conflicts":true,"future":1}"#,
        )
        .unwrap();
        assert!(!s.enabled);
        assert_eq!(s.pipeline_depth, PipelineDepth::Override(3));
        assert_eq!(s.pipeline_depth.blocks(false), 3);
        assert_eq!(s.idle_exit_s, 5);
        assert_eq!(s.gpu_warm_s, 7);
        assert_eq!(s.profile(), Profile::Battery);
        assert_eq!(s.pins["mic-denoise"], "m");
        assert!(s.forced_conflicts);
        assert_eq!(s.gpu_policy(), GpuPolicy::Never);
        assert!(!s.camera_active);
    }

    #[test]
    fn gpu_policy_and_camera() {
        let s = Snapshot::parse(r#"{"version":1,"gpu_policy":"with-camera","camera_active":true}"#)
            .unwrap();
        assert_eq!(s.gpu_policy(), GpuPolicy::WithCamera);
        assert!(s.camera_active);
        let d = Snapshot::default();
        assert_eq!(d.gpu_policy(), GpuPolicy::Always);
        assert_eq!(GpuPolicy::parse("bogus"), GpuPolicy::Always);
        for p in [GpuPolicy::Always, GpuPolicy::WithCamera, GpuPolicy::Never] {
            assert_eq!(GpuPolicy::parse(p.as_str()), p);
        }
    }

    #[test]
    fn depth_by_device_and_clamped() {
        let s = Snapshot::parse(r#"{"version":1,"pipeline_depth":{"cpu":1,"gpu":9}}"#).unwrap();
        assert_eq!(s.pipeline_depth.blocks(false), 1);
        assert_eq!(s.pipeline_depth.blocks(true), obc_audio::MAX_PIPELINE_DEPTH);
        let d = Snapshot::default();
        assert_eq!(d.pipeline_depth.blocks(false), 1);
        assert_eq!(d.pipeline_depth.blocks(true), 2);
    }

    #[test]
    fn missing_corrupt_or_other_version_gives_the_defaults_with_a_note() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(FILE);
        let (s, note) = Snapshot::load(&p);
        assert_eq!(s, Snapshot::default());
        assert!(note.unwrap().contains("no audio settings"));
        std::fs::write(&p, "{not json").unwrap();
        let (s, note) = Snapshot::load(&p);
        assert_eq!(s, Snapshot::default());
        assert!(note.unwrap().contains("cannot be used"));
        std::fs::write(&p, r#"{"version":2}"#).unwrap();
        let (_, note) = Snapshot::load(&p);
        assert!(note.unwrap().contains("version 2"));
        std::fs::write(&p, "[1]").unwrap();
        let (s, note) = Snapshot::load(&p);
        assert_eq!(s, Snapshot::default());
        assert!(note.unwrap().contains("not a JSON object"));
    }

    #[test]
    fn one_bad_field_keeps_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(FILE);
        std::fs::write(
            &p,
            r#"{"version":1,"idle_exit_s":"soon","gpu_warm_s":-1,"enabled":false,
                "disabled_modules":["a.b"],"pins":7,"future":{"x":1}}"#,
        )
        .unwrap();
        let (s, note) = Snapshot::load(&p);
        assert_eq!(s.disabled_modules, ["a.b"]);
        assert!(!s.enabled);
        assert_eq!(s.idle_exit_s, DEFAULT_IDLE_EXIT_S);
        assert_eq!(s.gpu_warm_s, DEFAULT_GPU_WARM_S);
        assert!(s.pins.is_empty());
        let note = note.unwrap();
        assert!(
            note.contains("idle_exit_s") && note.contains("gpu_warm_s") && note.contains("pins"),
            "{note}"
        );
        // ApplySnapshot stays strict.
        assert!(Snapshot::parse(r#"{"version":1,"pins":7}"#).is_err());
    }

    #[test]
    fn the_seconds_are_clamped_to_a_day() {
        let s =
            Snapshot::parse(r#"{"version":1,"idle_exit_s":999999,"gpu_warm_s":999999}"#).unwrap();
        assert_eq!(s.idle_exit_s, MAX_IDLE_EXIT_S);
        assert_eq!(s.gpu_warm_s, MAX_GPU_WARM_S);
        let d = Snapshot::default();
        assert_eq!(d.gpu_warm_s, DEFAULT_GPU_WARM_S);
        let s = Snapshot::parse(r#"{"version":1,"gpu_warm_s":0}"#).unwrap();
        assert_eq!(s.gpu_warm_s, 0);
    }

    #[test]
    fn a_file_over_the_cap_gives_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(FILE);
        let mut text = String::from(r##"{"version":1,"enabled":false,"pad":""##);
        text.push_str(&" ".repeat(MAX_FILE_SIZE as usize));
        text.push_str("\"}");
        std::fs::write(&p, text).unwrap();
        let (s, note) = Snapshot::load(&p);
        assert_eq!(s, Snapshot::default());
        assert!(note.unwrap().contains("larger than"));
    }
}
