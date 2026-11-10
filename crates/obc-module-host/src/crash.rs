//! The crash ledger: which modules were running their code when the daemon
//! last went down, so that a module that takes the daemon down twice in a
//! row is disabled until the user enables it again.
//!
//! The daemon calls [`CrashLedger::begin`] before a module's code runs in a
//! pipeline (or a probe) and [`CrashLedger::end`] when it stops cleanly;
//! while it runs, the ledger file holds a marker for the module. At start,
//! [`CrashLedger::on_start`] counts, per module, the consecutive starts
//! that found its marker (a start that finds none, or a clean stop, resets
//! the count) and clears the markers; at two in a row the module is
//! disabled with [`REASON_CRASHED`]. [`CrashLedger::reenable`] clears that.
//!
//! The file is JSON, written atomically (a temporary file in the same
//! directory, then a rename). A file that cannot be parsed is treated as
//! empty: losing the ledger only loses the count.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The reason of a module the ledger disabled.
pub const REASON_CRASHED: &str = "Disabled after crashing the daemon twice in a row";

/// How many consecutive starts that find a module's marker disable it.
pub const CRASHES_TO_DISABLE: u32 = 2;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct State {
    /// The modules whose code runs now, with how many runs.
    running: BTreeMap<String, u32>,
    /// The consecutive starts that found each module's marker.
    strikes: BTreeMap<String, u32>,
    /// The modules disabled, with the reason.
    disabled: BTreeMap<String, String>,
}

/// The crash ledger over one file. See the module documentation.
#[derive(Debug)]
pub struct CrashLedger {
    path: PathBuf,
    state: State,
    load_error: Option<String>,
}

/// The directory that holds `path`: for a bare file name, whose parent is
/// empty, the current directory.
fn directory_of(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

impl CrashLedger {
    /// The ledger in `path` (which need not exist yet). A file that cannot
    /// be read or parsed counts as empty, and says why
    /// ([`CrashLedger::load_error`]).
    pub fn open(path: impl Into<PathBuf>) -> CrashLedger {
        let path = path.into();
        let (state, load_error) = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(state) => (state, None),
                Err(e) => (
                    State::default(),
                    Some(format!(
                        "the crash ledger {} is damaged: {e}",
                        path.display()
                    )),
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (State::default(), None),
            Err(e) => (
                State::default(),
                Some(format!(
                    "the crash ledger {} could not be read: {e}",
                    path.display()
                )),
            ),
        };
        CrashLedger {
            path,
            state,
            load_error,
        }
    }

    /// Why the file was not read, if it was not.
    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    /// The file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn save(&self) -> std::io::Result<()> {
        let dir = directory_of(&self.path);
        std::fs::create_dir_all(dir)?;
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "crash-ledger".into());
        let tmp = dir.join(format!(".{name}.tmp"));
        let text = serde_json::to_string_pretty(&self.state).expect("the ledger serialises");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        // The rename itself is durable only once the directory is: a crash
        // right after it must not bring the previous ledger back.
        std::fs::File::open(dir)?.sync_all()
    }

    /// At daemon start: counts a strike for every module whose marker is
    /// set, resets the others, clears the markers and disables the modules
    /// at [`CRASHES_TO_DISABLE`] strikes. Returns the modules disabled by
    /// this start.
    pub fn on_start(&mut self) -> std::io::Result<Vec<String>> {
        let running = std::mem::take(&mut self.state.running);
        let strikes = std::mem::take(&mut self.state.strikes);
        let mut newly = Vec::new();
        for id in running.into_keys() {
            let n = strikes.get(&id).copied().unwrap_or(0) + 1;
            if n >= CRASHES_TO_DISABLE {
                if !self.state.disabled.contains_key(&id) {
                    newly.push(id.clone());
                }
                self.state.disabled.insert(id, REASON_CRASHED.into());
            } else {
                self.state.strikes.insert(id, n);
            }
        }
        self.save()?;
        Ok(newly)
    }

    /// Before module `id`'s code runs: sets its marker.
    pub fn begin(&mut self, id: &str) -> std::io::Result<()> {
        *self.state.running.entry(id.to_string()).or_insert(0) += 1;
        self.save()
    }

    /// When module `id`'s code stopped cleanly: clears one run's marker
    /// and, as it ran cleanly, its strikes.
    pub fn end(&mut self, id: &str) -> std::io::Result<()> {
        if let Some(n) = self.state.running.get_mut(id) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.state.running.remove(id);
            }
        }
        self.state.strikes.remove(id);
        self.save()
    }

    /// When module `id`'s probe (or other short call made at every start)
    /// returned: clears one run's marker but keeps its strikes, so that a
    /// clean probe at start does not forgive a crash of its pipeline code.
    pub fn end_probe(&mut self, id: &str) -> std::io::Result<()> {
        if let Some(n) = self.state.running.get_mut(id) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.state.running.remove(id);
            }
        }
        self.save()
    }

    /// Why the ledger disabled module `id`, if it did.
    pub fn disabled(&self, id: &str) -> Option<&str> {
        self.state.disabled.get(id).map(String::as_str)
    }

    /// Every module the ledger disabled, with the reason.
    pub fn all_disabled(&self) -> &BTreeMap<String, String> {
        &self.state.disabled
    }

    /// The consecutive starts that found module `id`'s marker.
    pub fn strikes(&self, id: &str) -> u32 {
        self.state.strikes.get(id).copied().unwrap_or(0)
    }

    /// Whether module `id`'s marker is set.
    pub fn is_running(&self, id: &str) -> bool {
        self.state.running.contains_key(id)
    }

    /// The user enables module `id` again: forgets its strikes and its
    /// disabling. Whether it was disabled.
    pub fn reenable(&mut self, id: &str) -> std::io::Result<bool> {
        let was = self.state.disabled.remove(id).is_some();
        self.state.strikes.remove(id);
        self.save()?;
        Ok(was)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_file_name_lives_in_the_current_directory() {
        assert_eq!(directory_of(Path::new("crash-ledger")), Path::new("."));
        assert_eq!(directory_of(Path::new("a/crash-ledger")), Path::new("a"));
        assert_eq!(directory_of(Path::new("/crash-ledger")), Path::new("/"));
    }

    const M: &str = "org.x.crashy";

    /// A daemon start: opens the ledger and runs `on_start`.
    fn start(path: &Path) -> (CrashLedger, Vec<String>) {
        let mut l = CrashLedger::open(path);
        let newly = l.on_start().unwrap();
        (l, newly)
    }

    #[test]
    fn two_crashes_in_a_row_disable_the_module() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/modules-crash.json");
        let (mut l, newly) = start(&path);
        assert!(newly.is_empty());
        l.begin(M).unwrap();
        assert!(l.is_running(M));
        drop(l); // the daemon dies with the module's code running

        let (mut l, newly) = start(&path);
        assert!(newly.is_empty());
        assert_eq!(l.strikes(M), 1);
        assert!(!l.is_running(M), "markers are cleared at start");
        l.begin(M).unwrap();
        drop(l); // and again

        let (l, newly) = start(&path);
        assert_eq!(newly, [M]);
        assert_eq!(l.disabled(M), Some(REASON_CRASHED));
        // It stays disabled across starts, reported once.
        drop(l);
        let (mut l, newly) = start(&path);
        assert!(newly.is_empty());
        assert_eq!(l.disabled(M), Some(REASON_CRASHED));
        assert_eq!(l.all_disabled().len(), 1);

        assert!(l.reenable(M).unwrap());
        assert!(!l.reenable(M).unwrap());
        drop(l);
        let (l, _) = start(&path);
        assert_eq!(l.disabled(M), None);
        assert_eq!(l.strikes(M), 0);
    }

    #[test]
    fn a_start_without_the_marker_or_a_clean_stop_resets_the_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        let (mut l, _) = start(&path);
        l.begin(M).unwrap();
        drop(l);
        let (l, _) = start(&path);
        assert_eq!(l.strikes(M), 1);
        drop(l);
        // A start that does not find the marker breaks the row.
        let (mut l, _) = start(&path);
        assert_eq!(l.strikes(M), 0);
        l.begin(M).unwrap();
        drop(l);
        let (mut l, newly) = start(&path);
        assert!(newly.is_empty());
        assert_eq!(l.strikes(M), 1);
        // A clean run forgets the strike too.
        l.begin(M).unwrap();
        l.end(M).unwrap();
        assert_eq!(l.strikes(M), 0);
        assert!(!l.is_running(M));
    }

    #[test]
    fn nested_runs_keep_the_marker_until_the_last_ends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        let mut l = CrashLedger::open(&path);
        l.begin(M).unwrap();
        l.begin(M).unwrap();
        l.end(M).unwrap();
        assert!(l.is_running(M));
        l.end(M).unwrap();
        assert!(!l.is_running(M));
        l.end(M).unwrap();
        assert!(!l.is_running(M), "an extra end is harmless");
        // Written atomically: no temporary file is left.
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["ledger.json"]);
    }

    #[test]
    fn a_clean_probe_keeps_the_strikes_of_a_crashed_pipeline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        for round in 0..2 {
            let (mut l, newly) = start(&path);
            assert!(newly.is_empty(), "round {round}");
            // The probe at start runs cleanly...
            l.begin(M).unwrap();
            l.end_probe(M).unwrap();
            assert!(!l.is_running(M));
            assert_eq!(l.strikes(M), round);
            // ...then the pipeline's code takes the daemon down.
            l.begin(M).unwrap();
        }
        let (l, newly) = start(&path);
        assert_eq!(newly, [M]);
        assert_eq!(l.disabled(M), Some(REASON_CRASHED));
    }

    #[test]
    fn a_damaged_file_counts_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        std::fs::write(&path, "{not json").unwrap();
        let mut l = CrashLedger::open(&path);
        assert!(l.load_error().unwrap().contains("damaged"));
        assert!(l.on_start().unwrap().is_empty());
        let l = CrashLedger::open(&path);
        assert_eq!(l.load_error(), None, "rewritten whole");
    }
}
