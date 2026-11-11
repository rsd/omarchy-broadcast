//! The configuration as a set of files: `config.toml`, then the drop-ins of
//! `config.d/` in lexical order, each later file winning key by key
//! ([`ConfigSet`]); the one drop-in the daemon writes,
//! `config.d/50-panel.toml` ([`persist_managed`], [`set_config`],
//! [`reset_config`]); and the move of the module choices from the state
//! file into it ([`migrate_modules_state`]).
//!
//! # Loading
//!
//! The files are `config.toml`, then every regular file of `config.d/`
//! whose name ends in `.toml` and does not start with `.`, in byte order of
//! the name (a missing directory has none). Each file is read alone first,
//! as a [`Config`], so that an unknown key, a value of the wrong type or out
//! of range is reported with that file and its line. The TOML tables are
//! then merged: tables merge key by key, any other value (a scalar, an
//! array) is replaced whole by the later file. The merged table is read as
//! a [`Config`] and checked as a whole ([`Config::validate`]); an error
//! there names the file that set the key. Any invalid file rejects the
//! whole set: the caller keeps the configuration in force.
//!
//! For every leaf key (`output.mode`, `modules.pin.background`,
//! `modules.enabled.<id>`) the set records the file that set the value in
//! force ([`ConfigSet::source`]), as a path relative to the configuration
//! directory.
//!
//! # The managed file
//!
//! Only the daemon writes `config.d/50-panel.toml`, and only through
//! [`persist_managed`]: the new merged configuration is checked as a load
//! would check it, then the whole file is generated from the managed table
//! (a header comment, then the keys) and written atomically (a temporary
//! file in the same directory, flushed, renamed over the file, the
//! directory flushed). `config.toml` and every other file are never
//! written. A managed file that does not load is never overwritten: the
//! write is refused ([`WriteError::ManagedUnreadable`]) until the user
//! fixes or deletes it. A key that a file after the managed one sets is
//! refused too ([`WriteError::Overridden`]), unless the caller says
//! otherwise ([`OverridePolicy::Ignore`], for the migration).

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::config::{
    Config, ConfigError, RuntimeState, create_temp, line_of_key, parse_error, remove_stale_temps,
};
use crate::config_schema::{self, ConfigValue, RegistryFacts};
use crate::modules::{
    ModulesState, not_a_module_id, not_a_stage_id, valid_module_id, valid_stage_id,
};

/// The name of the main configuration file in the configuration directory.
pub const CONFIG_FILE: &str = "config.toml";
/// The drop-in directory, relative to the configuration directory.
pub const CONFIG_D: &str = "config.d";
/// The file name of the managed drop-in in [`CONFIG_D`].
pub const MANAGED_FILE: &str = "50-panel.toml";
/// The managed drop-in, relative to the configuration directory.
pub const MANAGED_REL: &str = "config.d/50-panel.toml";
/// The first lines of the managed drop-in.
pub const MANAGED_HEADER: &str = "# Managed by omarchy-broadcastd (the bar's setup dialog and the CLI). Do not edit:\n\
     # put your own settings in config.toml or in a later config.d file such as 90-local.toml, which wins.\n";

/// The state of one file of the set, for diagnostics
/// ([`config_files`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    /// Read and valid.
    Loaded,
    /// Not there (only `config.toml` and the managed file are listed when
    /// absent).
    Absent,
    /// It cannot be used; the reason says why.
    Error,
}

impl FileStatus {
    /// The wire name: `loaded`, `absent` or `error`.
    pub fn as_str(self) -> &'static str {
        match self {
            FileStatus::Loaded => "loaded",
            FileStatus::Absent => "absent",
            FileStatus::Error => "error",
        }
    }
}

/// One file of the set, in load order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFile {
    /// Relative to the configuration directory: `config.toml`,
    /// `config.d/50-panel.toml`.
    pub path: String,
    pub status: FileStatus,
    /// The daemon writes it ([`MANAGED_REL`]).
    pub managed: bool,
    /// Why it cannot be used (with [`FileStatus::Error`]), else empty:
    /// the position and the message, without the path.
    pub reason: String,
}

/// One file read and checked alone.
#[derive(Debug, Clone)]
struct Layer {
    rel: String,
    path: PathBuf,
    text: String,
    table: toml::Table,
}

/// The configuration loaded from every file of the set.
#[derive(Debug, Clone)]
pub struct ConfigSet {
    /// The merged, checked configuration.
    pub config: Config,
    /// The configuration directory (the parent of `config.toml`).
    pub dir: PathBuf,
    /// Every file in load order, `config.toml` and the managed file
    /// included when absent. All are [`FileStatus::Loaded`] or
    /// [`FileStatus::Absent`] in a loaded set.
    pub files: Vec<ConfigFile>,
    /// The file that set the value in force, by leaf key path, relative to
    /// [`Self::dir`]. A key that no file sets is not here (its default is
    /// in force).
    pub provenance: BTreeMap<String, String>,
    /// The table of the managed file, empty when it is absent.
    pub managed: toml::Table,
    layers: Vec<Layer>,
}

/// The directory holding `config_toml` (`.` for a bare file name).
fn config_dir(config_toml: &Path) -> PathBuf {
    match config_toml.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// The path of the managed file for the configuration file `config_toml`.
pub fn managed_path(config_toml: &Path) -> PathBuf {
    config_dir(config_toml).join(CONFIG_D).join(MANAGED_FILE)
}

/// The segments of a dotted key path. The last segment of
/// `modules.enabled.<id>` and `modules.pin.<stage>` is kept whole (a module
/// id holds dots); any other key splits at every dot.
pub fn key_segments(key: &str) -> Vec<&str> {
    for (table, sub) in [("modules", "enabled"), ("modules", "pin")] {
        let prefix_len = table.len() + sub.len() + 2;
        if key.len() > prefix_len
            && key.starts_with(table)
            && key[table.len()..].starts_with('.')
            && key[table.len() + 1..].starts_with(sub)
            && key[table.len() + 1 + sub.len()..].starts_with('.')
        {
            return vec![table, sub, &key[prefix_len..]];
        }
    }
    key.split('.').collect()
}

/// The value at the dotted `key` in `table`, if any.
pub fn table_get<'t>(table: &'t toml::Table, key: &str) -> Option<&'t toml::Value> {
    let segments = key_segments(key);
    let (last, parents) = segments.split_last()?;
    let mut t = table;
    for s in parents {
        t = t.get(*s)?.as_table()?;
    }
    t.get(*last)
}

/// The names of the drop-ins of `dir` in load order: regular files whose
/// UTF-8 name ends in `.toml` and does not start with `.`.
fn drop_in_names(dir: &Path) -> Result<Vec<String>, ConfigError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(ConfigError::Read {
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let wanted = !name.starts_with('.')
                && name.ends_with(".toml")
                && fs::metadata(e.path()).is_ok_and(|m| m.is_file());
            wanted.then_some(name)
        })
        .collect();
    // Byte order of the names (`str` orders by bytes).
    names.sort();
    Ok(names)
}

/// Reads and checks one file alone: `None` when it is absent.
fn read_layer(dir: &Path, rel: &str) -> Result<Option<Layer>, ConfigError> {
    let path = dir.join(rel);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(ConfigError::Read { path, source }),
    };
    parse_layer(rel, path, text).map(Some)
}

/// Checks `text` as one file of the set: every key and value as a
/// [`Config`] reads them (the checks spanning keys run on the merged set).
fn parse_layer(rel: &str, path: PathBuf, text: String) -> Result<Layer, ConfigError> {
    toml::from_str::<Config>(&text).map_err(|e| parse_error(&e, &text, &path))?;
    let table = toml::from_str::<toml::Table>(&text).map_err(|e| parse_error(&e, &text, &path))?;
    Ok(Layer {
        rel: rel.to_string(),
        path,
        text,
        table,
    })
}

/// Merges `top` into `base`: tables key by key, any other value replaced.
fn merge_into(base: &mut toml::Table, top: &toml::Table) {
    for (k, v) in top {
        match (base.get_mut(k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(t)) => merge_into(b, t),
            _ => {
                base.insert(k.clone(), v.clone());
            }
        }
    }
}

/// Records `rel` as the file of every leaf of `table` under `prefix`.
fn record(provenance: &mut BTreeMap<String, String>, prefix: &str, table: &toml::Table, rel: &str) {
    for (k, v) in table {
        let path = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        match v {
            toml::Value::Table(t) => record(provenance, &path, t, rel),
            _ => {
                provenance.insert(path, rel.to_string());
            }
        }
    }
}

/// Whether `table` has a leaf at `key` or under it.
fn sets_key(table: &toml::Table, key: &str) -> bool {
    table_get(table, key).is_some()
}

/// The merged configuration of `layers` and its provenance.
fn assemble(
    dir: &Path,
    layers: &[&Layer],
) -> Result<(Config, BTreeMap<String, String>), ConfigError> {
    let mut merged = toml::Table::new();
    let mut provenance = BTreeMap::new();
    for layer in layers {
        merge_into(&mut merged, &layer.table);
        record(&mut provenance, "", &layer.table, &layer.rel);
    }
    let config: Config = toml::Value::Table(merged)
        .try_into()
        .map_err(|e: toml::de::Error| ConfigError::Parse {
            path: dir.join(CONFIG_FILE),
            line: None,
            column: None,
            key: None,
            message: e.message().trim().to_string(),
        })?;
    config.validate().map_err(|e| match e {
        ConfigError::Invalid { key, reason } => {
            // The last file that sets the key (or a key under it).
            let layer = layers.iter().rev().find(|l| sets_key(&l.table, key));
            let (path, line) = match layer {
                Some(l) => (l.path.clone(), line_of(&l.text, key)),
                None => (dir.join(CONFIG_FILE), None),
            };
            ConfigError::Parse {
                path,
                line,
                column: None,
                key: Some(key.to_string()),
                message: reason,
            }
        }
        other => other,
    })?;
    Ok((config, provenance))
}

/// The 1-based line of `key` in `text`: its `key = ...` line, else its
/// `[key]` header.
fn line_of(text: &str, key: &str) -> Option<usize> {
    line_of_key(text, key).or_else(|| {
        text.lines()
            .position(|l| {
                l.trim()
                    .strip_prefix('[')
                    .and_then(|r| r.split(']').next())
                    .is_some_and(|h| h.trim() == key)
            })
            .map(|i| i + 1)
    })
}

/// The error text without the file's path: `line 3, column 5: key:
/// message`, for [`ConfigFile::reason`].
pub fn error_reason(e: &ConfigError) -> String {
    match e {
        ConfigError::Read { source, .. } => source.to_string(),
        ConfigError::Parse {
            line,
            column,
            key,
            message,
            ..
        } => {
            let mut out = String::new();
            match (line, column) {
                (Some(l), Some(c)) => out.push_str(&format!("line {l}, column {c}: ")),
                (Some(l), None) => out.push_str(&format!("line {l}: ")),
                _ => {}
            }
            if let Some(k) = key {
                out.push_str(&format!("{k}: "));
            }
            out.push_str(message);
            out
        }
        ConfigError::Invalid { key, reason } => format!("{key}: {reason}"),
    }
}

/// The error text without the path and position: `key: message`, the
/// reason of a refused write.
fn error_message(e: &ConfigError) -> String {
    match e {
        ConfigError::Parse {
            key: Some(k),
            message,
            ..
        } => format!("{k}: {message}"),
        ConfigError::Parse { message, .. } => message.clone(),
        other => error_reason(other),
    }
}

/// Every file of the set read, and the result of the whole load.
struct Scan {
    dir: PathBuf,
    files: Vec<ConfigFile>,
    layers: Vec<Layer>,
    error: Option<ConfigError>,
}

fn scan(config_toml: &Path) -> Scan {
    let dir = config_dir(config_toml);
    let mut files = Vec::new();
    let mut layers = Vec::new();
    let mut error = None;
    let mut rels = vec![CONFIG_FILE.to_string()];
    let mut names = match drop_in_names(&dir.join(CONFIG_D)) {
        Ok(names) => names,
        Err(e) => {
            files.push(ConfigFile {
                path: CONFIG_D.to_string(),
                status: FileStatus::Error,
                managed: false,
                reason: error_reason(&e),
            });
            error = Some(e);
            Vec::new()
        }
    };
    if !names.iter().any(|n| n == MANAGED_FILE) {
        names.push(MANAGED_FILE.to_string());
        names.sort();
    }
    rels.extend(names.into_iter().map(|n| format!("{CONFIG_D}/{n}")));
    for rel in rels {
        let managed = rel == MANAGED_REL;
        let (status, reason) = match read_layer(&dir, &rel) {
            Ok(Some(layer)) => {
                layers.push(layer);
                (FileStatus::Loaded, String::new())
            }
            Ok(None) => (FileStatus::Absent, String::new()),
            Err(e) => {
                let reason = error_reason(&e);
                error.get_or_insert(e);
                (FileStatus::Error, reason)
            }
        };
        files.push(ConfigFile {
            path: rel,
            status,
            managed,
            reason,
        });
    }
    if error.is_none() {
        let refs: Vec<&Layer> = layers.iter().collect();
        if let Err(e) = assemble(&dir, &refs) {
            // The file the error names is the one in error.
            if let ConfigError::Parse { path, .. } = &e
                && let Some(f) = files.iter_mut().find(|f| dir.join(&f.path) == *path)
            {
                f.status = FileStatus::Error;
                f.reason = error_reason(&e);
            }
            error = Some(e);
        }
    }
    Scan {
        dir,
        files,
        layers,
        error,
    }
}

/// The files of the set for `config_toml` in load order, each with its
/// state, and the configuration directory. Never fails: a file that cannot
/// be used is [`FileStatus::Error`] with the reason (when every file reads
/// alone but the merged set does not, the file the error names).
pub fn config_files(config_toml: &Path) -> (PathBuf, Vec<ConfigFile>) {
    let s = scan(config_toml);
    (s.dir, s.files)
}

impl ConfigSet {
    /// Loads the set of `config_toml` (`.../omarchy-broadcast/config.toml`):
    /// that file, then the drop-ins of `config.d/` next to it. Missing
    /// files are none; the first file that cannot be used (in load order),
    /// or the merged set failing its checks, is the error, naming the file
    /// and the line.
    pub fn load(config_toml: &Path) -> Result<ConfigSet, ConfigError> {
        let s = scan(config_toml);
        if let Some(e) = s.error {
            return Err(e);
        }
        let refs: Vec<&Layer> = s.layers.iter().collect();
        let (config, provenance) = assemble(&s.dir, &refs)?;
        let managed = s
            .layers
            .iter()
            .find(|l| l.rel == MANAGED_REL)
            .map(|l| l.table.clone())
            .unwrap_or_default();
        Ok(ConfigSet {
            config,
            dir: s.dir,
            files: s.files,
            provenance,
            managed,
            layers: s.layers,
        })
    }

    /// The built-in defaults standing for the set of `config_toml`, for a
    /// daemon whose set did not load: no file sets any key, the files are
    /// as [`config_files`] finds them (the bad one with its error).
    pub fn defaults(config_toml: &Path) -> ConfigSet {
        let s = scan(config_toml);
        ConfigSet {
            config: Config::default(),
            dir: s.dir,
            files: s.files,
            provenance: BTreeMap::new(),
            managed: toml::Table::new(),
            layers: Vec::new(),
        }
    }

    /// The file that sets the value in force of the leaf `key`, relative
    /// to [`Self::dir`]; `""` when no file does (the default).
    pub fn source(&self, key: &str) -> &str {
        self.provenance.get(key).map_or("", String::as_str)
    }

    /// The file after the managed one that sets `key` (the value in force
    /// comes from it, so a write to the managed file would not show);
    /// `""` when none does.
    pub fn overridden_by(&self, key: &str) -> &str {
        let source = self.source(key);
        match source
            .strip_prefix(CONFIG_D)
            .and_then(|r| r.strip_prefix('/'))
        {
            Some(name) if name > MANAGED_FILE => source,
            _ => "",
        }
    }

    /// The path of the managed file.
    pub fn managed_path(&self) -> PathBuf {
        self.dir.join(CONFIG_D).join(MANAGED_FILE)
    }

    /// The configuration this set would load with `managed` as the
    /// managed file's table, checked as a load checks it (the managed text
    /// is the one [`managed_text`] writes).
    pub fn with_managed(&self, managed: &toml::Table) -> Result<Config, ConfigError> {
        self.with_managed_set(managed).map(|s| s.config)
    }

    /// The whole set this set would load with `managed` as the managed
    /// file's table (that file loaded): [`Self::with_managed`] with the
    /// provenance, the files and the layers to match.
    pub fn with_managed_set(&self, managed: &toml::Table) -> Result<ConfigSet, ConfigError> {
        let text = managed_text(managed);
        let managed_layer = parse_layer(MANAGED_REL, self.managed_path(), text)?;
        let mut layers: Vec<Layer> = self
            .layers
            .iter()
            .filter(|l| l.rel != MANAGED_REL)
            .cloned()
            .collect();
        let at = layers
            .iter()
            .position(|l| l.rel.as_str() > MANAGED_REL && l.rel != CONFIG_FILE)
            .unwrap_or(layers.len());
        layers.insert(at, managed_layer);
        let refs: Vec<&Layer> = layers.iter().collect();
        let (config, provenance) = assemble(&self.dir, &refs)?;
        let mut files = self.files.clone();
        for f in files.iter_mut().filter(|f| f.managed) {
            f.status = FileStatus::Loaded;
            f.reason.clear();
        }
        Ok(ConfigSet {
            config,
            dir: self.dir.clone(),
            files,
            provenance,
            managed: managed.clone(),
            layers,
        })
    }
}

/// A change of the managed table.
#[derive(Debug, Clone, PartialEq)]
pub enum ManagedEdit {
    /// Sets the dotted key ([`key_segments`]) to the value.
    Set(String, toml::Value),
    /// Removes the dotted key (and the tables it leaves empty).
    Remove(String),
}

impl ManagedEdit {
    /// The key the edit changes.
    pub fn key(&self) -> &str {
        match self {
            ManagedEdit::Set(k, _) | ManagedEdit::Remove(k) => k,
        }
    }
}

/// Whether a write may change a key that a file after the managed one
/// sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverridePolicy {
    /// Refused with [`WriteError::Overridden`]: every write the user asks
    /// for (`SetConfig`, `ResetConfig`, `SetModuleEnabled`,
    /// `SetStageOrder`, `SetBackendProfile`, `SetStagePin`).
    Refuse,
    /// Written anyway: the migration of the state file's choices, which
    /// moves values rather than changing them.
    Ignore,
}

/// A refused or failed write of the managed file. The daemon answers
/// `UnknownSetting` for [`Self::UnknownKey`], `Disabled` for
/// [`Self::NotEditable`] and [`Self::Overridden`], `InvalidArgument` for
/// [`Self::Invalid`], `InvalidConfig` for [`Self::Config`] and `Failed`
/// for [`Self::ManagedUnreadable`] and [`Self::Io`].
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// No such configuration key (or no such module or stage).
    #[error("No configuration key {0}")]
    UnknownKey(String),
    /// The key is not changed through the daemon.
    #[error("{0} cannot be changed here: set it in config.toml")]
    NotEditable(String),
    /// A file after the managed one sets the key.
    #[error("Overridden by {file}")]
    Overridden { key: String, file: String },
    /// The value is refused: the reason, naming the key.
    #[error("{0}")]
    Invalid(String),
    /// The managed file exists but does not load, so it is not
    /// overwritten.
    #[error("{file} is not written because it cannot be read ({reason}): fix or delete it")]
    ManagedUnreadable { file: String, reason: String },
    /// Another file of the set cannot be used, so no new configuration
    /// can be checked.
    #[error("{0}")]
    Config(ConfigError),
    /// Writing the managed file failed.
    #[error("{}: {source}", .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// `managed` with `edits` applied in order.
pub fn apply_edits(managed: &toml::Table, edits: &[ManagedEdit]) -> toml::Table {
    let mut out = managed.clone();
    for edit in edits {
        let segments = key_segments(edit.key());
        match edit {
            ManagedEdit::Set(_, value) => set_at(&mut out, &segments, value.clone()),
            ManagedEdit::Remove(_) => {
                remove_at(&mut out, &segments);
            }
        }
    }
    out
}

fn set_at(table: &mut toml::Table, segments: &[&str], value: toml::Value) {
    match segments {
        [] => {}
        [last] => {
            table.insert((*last).to_string(), value);
        }
        [first, rest @ ..] => {
            let entry = table
                .entry((*first).to_string())
                .or_insert_with(|| toml::Value::Table(toml::Table::new()));
            if !entry.is_table() {
                *entry = toml::Value::Table(toml::Table::new());
            }
            if let toml::Value::Table(t) = entry {
                set_at(t, rest, value);
            }
        }
    }
}

/// Removes the key; whether `table` is empty afterwards.
fn remove_at(table: &mut toml::Table, segments: &[&str]) -> bool {
    match segments {
        [] => {}
        [last] => {
            table.remove(*last);
        }
        [first, rest @ ..] => {
            if let Some(toml::Value::Table(t)) = table.get_mut(*first)
                && remove_at(t, rest)
            {
                table.remove(*first);
            }
        }
    }
    table.is_empty()
}

/// The text of the managed file for `managed`: [`MANAGED_HEADER`], then
/// the table.
pub fn managed_text(managed: &toml::Table) -> String {
    let body = toml::to_string(managed).expect("a TOML table always serializes");
    if body.is_empty() {
        MANAGED_HEADER.to_string()
    } else {
        format!("{MANAGED_HEADER}\n{body}")
    }
}

/// Applies `edits` to the managed file of `config_toml` and writes it, then
/// loads the set again and returns it. Refused, with nothing written, when
/// the managed file exists and does not load, another file of the set does
/// not load, an edited key is overridden (with [`OverridePolicy::Refuse`]),
/// or the new configuration fails a check. An edit that changes nothing
/// writes nothing. Creates `config.d/` when needed; never writes any other
/// file, and never replaces a managed file that is a symbolic link
/// ([`WriteError::Io`]). Once the file is written, a load of the set that
/// fails (a file changed meanwhile) does not undo the write: the set
/// checked before the write is returned.
pub fn persist_managed(
    config_toml: &Path,
    edits: &[ManagedEdit],
    policy: OverridePolicy,
) -> Result<ConfigSet, WriteError> {
    persist_with(config_toml, edits, policy, ConfigSet::load)
}

/// [`persist_managed`], loading the set after the write with `reload`.
fn persist_with(
    config_toml: &Path,
    edits: &[ManagedEdit],
    policy: OverridePolicy,
    reload: impl FnOnce(&Path) -> Result<ConfigSet, ConfigError>,
) -> Result<ConfigSet, WriteError> {
    let set = load_for_write(config_toml)?;
    if policy == OverridePolicy::Refuse
        && let Some((key, file)) = edits.iter().find_map(|e| {
            let file = set.overridden_by(e.key());
            (!file.is_empty()).then(|| (e.key().to_string(), file.to_string()))
        })
    {
        return Err(WriteError::Overridden { key, file });
    }
    let managed = apply_edits(&set.managed, edits);
    let path = set.managed_path();
    if managed == set.managed && (path.exists() || managed.is_empty()) {
        return Ok(set);
    }
    let checked = set
        .with_managed_set(&managed)
        .map_err(|e| WriteError::Invalid(error_message(&e)))?;
    if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(WriteError::Io {
            path,
            source: io::Error::other(
                "is a symbolic link, which the daemon does not replace: make it a plain \
                 file, or edit the file it points to",
            ),
        });
    }
    write_atomically(&path, managed_text(&managed).as_bytes())
        .map_err(|source| WriteError::Io { path, source })?;
    Ok(reload(config_toml).unwrap_or_else(|e| {
        eprintln!(
            "the managed config was written, but loading it back failed ({}); \
             using the set checked before the write",
            error_message(&e)
        );
        checked
    }))
}

/// The set, for a write: a managed file that does not load is
/// [`WriteError::ManagedUnreadable`], any other failure
/// [`WriteError::Config`].
fn load_for_write(config_toml: &Path) -> Result<ConfigSet, WriteError> {
    let s = scan(config_toml);
    if let Some(f) = s
        .files
        .iter()
        .find(|f| f.managed && f.status == FileStatus::Error)
    {
        return Err(WriteError::ManagedUnreadable {
            file: f.path.clone(),
            reason: f.reason.clone(),
        });
    }
    ConfigSet::load(config_toml).map_err(WriteError::Config)
}

/// Writes `bytes` to `path` atomically: a temporary hidden file in the same
/// directory (created when needed), flushed, renamed over `path`, then the
/// directory flushed. Also the daemon's writer of the audio settings
/// snapshot (`audio-settings.json`).
pub fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = config_dir(path);
    // The directories created here, each flushed into its parent.
    let mut created = Vec::new();
    let mut d = dir.as_path();
    while fs::symlink_metadata(d).is_err() {
        created.push(d.to_path_buf());
        match d.parent() {
            Some(p) if !p.as_os_str().is_empty() => d = p,
            _ => break,
        }
    }
    fs::create_dir_all(&dir)?;
    for c in created.iter().rev() {
        fs::File::open(config_dir(c))?.sync_all()?;
    }
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    let (tmp, f) = create_temp(&dir, name)?;
    let write = |mut f: fs::File| -> io::Result<()> {
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    };
    if let Err(e) = write(f) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    remove_stale_temps(&dir, name);
    // The file is replaced by now: a failed flush of the directory is not a
    // failed write.
    if let Err(e) = fs::File::open(&dir).and_then(|d| d.sync_all()) {
        eprintln!(
            "{} was replaced, but flushing its directory failed: {e}",
            path.display()
        );
    }
    Ok(())
}

/// `SetConfig(key, value)`: checks that `key` is a row of the config schema
/// ([`config_schema::lookup`]) that is editable, converts `value` to the
/// key's type ([`config_schema::to_toml`]), then [`persist_managed`] with
/// [`OverridePolicy::Refuse`].
pub fn set_config(
    config_toml: &Path,
    facts: &RegistryFacts,
    key: &str,
    value: &ConfigValue,
) -> Result<ConfigSet, WriteError> {
    let spec =
        config_schema::lookup(key, facts).ok_or_else(|| WriteError::UnknownKey(key.into()))?;
    if !spec.editable {
        return Err(WriteError::NotEditable(key.into()));
    }
    let value = config_schema::to_toml(key, &spec.kind, value).map_err(WriteError::Invalid)?;
    persist_managed(
        config_toml,
        &[ManagedEdit::Set(key.into(), value)],
        OverridePolicy::Refuse,
    )
}

/// `ResetConfig(key)`: removes `key` from the managed file, so that the
/// value of an earlier file (or the default) is in force. The same checks
/// as [`set_config`]; a key the managed file does not set writes nothing.
/// A value the managed file sets is removed whatever the schema says of
/// its key (an entry for a module no longer installed, or a stage no
/// longer offered, is cleared this way).
pub fn reset_config(
    config_toml: &Path,
    facts: &RegistryFacts,
    key: &str,
) -> Result<ConfigSet, WriteError> {
    let in_managed = || {
        load_for_write(config_toml)
            .is_ok_and(|set| table_get(&set.managed, key).is_some_and(|v| !v.is_table()))
    };
    match config_schema::lookup(key, facts) {
        Some(spec) if spec.editable => {}
        _ if in_managed() => {}
        Some(_) => return Err(WriteError::NotEditable(key.into())),
        None => return Err(WriteError::UnknownKey(key.into())),
    }
    persist_managed(
        config_toml,
        &[ManagedEdit::Remove(key.into())],
        OverridePolicy::Refuse,
    )
}

/// The module choices of a state file moved into the managed file
/// ([`migrate_modules_state`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ModulesMigration {
    /// The keys to add to the managed file: those of the state's
    /// `[modules]` that the managed file does not set already (it wins).
    pub edits: Vec<ManagedEdit>,
    /// The state without its `[modules]` table, to save once the edits are
    /// written.
    pub state: RuntimeState,
    /// One line per entry dropped because it is not valid (a module or
    /// stage id of the wrong form, a stage named twice in the order).
    pub notes: Vec<String>,
    had_choices: bool,
}

impl ModulesMigration {
    /// Whether the state had module choices to move (then the state is to
    /// be saved, even with no edits).
    pub fn is_needed(&self) -> bool {
        self.had_choices
    }
}

/// Moves the module choices of `state` (enabled modules, stage order,
/// backend profile, pins) into managed-file keys (`modules.enabled.<id>`,
/// `modules.order`, `modules.profile`, `modules.pin.<stage>`), each unless
/// `managed` (the managed file's table, [`ConfigSet::managed`]) sets it
/// already. Module setting values (`[settings]`) and every other key stay
/// in the state.
///
/// The daemon, on load: when [`ModulesMigration::is_needed`], first writes
/// the edits ([`persist_managed`] with [`OverridePolicy::Ignore`]; nothing
/// to write when there are none), then, only once that succeeded, saves
/// [`ModulesMigration::state`]. A crash between the two loses nothing: the
/// config holds the choices, and the state's copy is dropped by the next
/// migration (the managed file then sets every key, so no edit is made).
/// A failed write keeps the state as it is, to be tried on the next load.
pub fn migrate_modules_state(state: &RuntimeState, managed: &toml::Table) -> ModulesMigration {
    let m = &state.modules;
    let mut edits = Vec::new();
    let mut notes = Vec::new();
    let mut add = |key: String, value: toml::Value| {
        if table_get(managed, &key).is_none() {
            edits.push(ManagedEdit::Set(key, value));
        }
    };
    for (id, enabled) in &m.enabled {
        if valid_module_id(id) {
            add(
                format!("modules.enabled.{id}"),
                toml::Value::Boolean(*enabled),
            );
        } else {
            notes.push(format!("modules.enabled: {}: dropped", not_a_module_id(id)));
        }
    }
    if let Some(order) = &m.order {
        let bad = order
            .iter()
            .enumerate()
            .find(|(i, s)| !valid_stage_id(s) || order[..*i].contains(s));
        match bad {
            None => add(
                "modules.order".into(),
                toml::Value::Array(order.iter().cloned().map(toml::Value::String).collect()),
            ),
            Some((_, s)) => notes.push(format!(
                "modules.order: {}: the order is dropped",
                if valid_stage_id(s) {
                    format!("names the stage {s:?} twice")
                } else {
                    not_a_stage_id(s)
                }
            )),
        }
    }
    if let Some(profile) = m.profile {
        add(
            "modules.profile".into(),
            toml::Value::String(profile.as_str().into()),
        );
    }
    for (stage, module) in &m.pin {
        if !valid_stage_id(stage) {
            notes.push(format!("modules.pin: {}: dropped", not_a_stage_id(stage)));
        } else if !module.is_empty() && !valid_module_id(module) {
            notes.push(format!(
                "modules.pin.{stage}: {}: dropped",
                not_a_module_id(module)
            ));
        } else {
            add(
                format!("modules.pin.{stage}"),
                toml::Value::String(module.clone()),
            );
        }
    }
    let had_choices = !state.modules.is_empty();
    let mut state = state.clone();
    state.modules = ModulesState::default();
    ModulesMigration {
        edits,
        state,
        notes,
        had_choices,
    }
}

#[cfg(test)]
mod tests;
