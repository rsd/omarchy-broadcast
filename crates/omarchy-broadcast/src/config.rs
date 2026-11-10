//! `config`: the daemon's configuration (interface revision 10): every key
//! with its value, where the value comes from and whether the daemon
//! changes it (the config schema), one key's value, a change or a reset of
//! a key (written by the daemon to `config.d/50-panel.toml`), and the
//! configuration files in load order.
//!
//! The reads (`config`, `config show`, `config get`, `config files`) never
//! start the daemon; `config set` and `config reset` may, through D-Bus
//! activation. A change is printed as read back.
//!
//! JSON (`docs/cli-json.md`): `config show --json` is a set of
//! `config_group` lines, packed as the `settings_group` lines are
//! ([`config_lines`]); `config files --json` one `config_files` line
//! ([`files_line`]).

use std::borrow::Cow;

use clap::Subcommand;
use obc_ipc::{ConfigFiles, ConfigKind, ConfigRow, ConfigSchema, ConfigValue, IpcError};
use serde::Serialize;

use crate::bus::{self, EXIT_OK, EXIT_USAGE, Failure, READ_TIMEOUT};
use crate::human::printable;
use crate::json::{
    self, MAX_GROUP_ID_BYTES, MAX_LABEL_BYTES, MAX_PATH_BYTES, MAX_SETTINGS_PER_LINE,
    MAX_UNIT_BYTES, VERSION, clip, clip_start,
};
use crate::{CALL_TIMEOUT, out, report, within};

/// The version of the row model (that of the settings rows).
pub const CONFIG_SCHEMA: u32 = 1;
/// The longest key in JSON, in bytes.
const MAX_KEY_BYTES: usize = 128;
/// The longest description or reason in JSON, in bytes.
const MAX_DESCRIPTION_BYTES: usize = 160;
/// The longest string value, option value or option label in JSON, in
/// bytes (a module id, a path keeps its end).
const MAX_VALUE_BYTES: usize = 160;
/// At most this many options of an enum row.
const MAX_OPTIONS: usize = 16;
/// At most this many items of a list value.
const MAX_LIST_ITEMS: usize = 8;
/// At most this many files in a `config_files` line.
const MAX_FILES: usize = 16;

/// The subcommands of `config`.
#[derive(Subcommand, Clone)]
pub enum ConfigAction {
    /// Show every configuration key, by group: its value, the file that
    /// sets it (or the default), whether it can be changed here, and when
    /// a change takes effect.
    Show {
        /// Print JSON lines (`"type":"config_group"`).
        #[arg(long)]
        json: bool,
    },
    /// Print the value of one key, e.g. `output.hold_timeout_s`.
    Get {
        /// The dotted key.
        key: String,
    },
    /// Set a key: the daemon writes it to config.d/50-panel.toml and
    /// applies it; then print the key. The value is read by the key's
    /// type: `true` or `false`, a number, one of its options, a text, or a
    /// comma-separated list. Put `--` before the key when the value is
    /// negative.
    Set {
        /// The dotted key.
        key: String,
        /// The value.
        #[arg(allow_hyphen_values = true)]
        value: String,
    },
    /// Remove a key from config.d/50-panel.toml, so that the value of
    /// config.toml (or the default) is in force again; then print the key.
    Reset {
        /// The dotted key.
        key: String,
    },
    /// List the configuration files in load order, with their status.
    Files {
        /// Print one JSON line (`"type":"config_files"`).
        #[arg(long)]
        json: bool,
    },
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

/// `v` as text: a list comma-separated, a float in its shortest form.
pub fn value_text(v: &ConfigValue) -> String {
    match v {
        ConfigValue::Bool(b) => b.to_string(),
        ConfigValue::Int(i) => i.to_string(),
        ConfigValue::Float(f) => f.to_string(),
        ConfigValue::Str(s) => s.clone(),
        ConfigValue::List(items) => items.join(","),
    }
}

/// The value `text` names for `row`, typed by its type: `true`/`false`
/// (also `on`/`off`) for a bool, a whole number for an int, a number for a
/// float, the text itself for an enum, a string or a path, a
/// comma-separated list (items trimmed, empty ones dropped) for a list.
/// `Err` is the usage message. An enum's option and every range are the
/// daemon's to check.
pub fn typed_value(row: &ConfigRow, text: &str) -> Result<ConfigValue, String> {
    let id = printable(&row.id);
    let shown = printable(text);
    match &row.kind {
        ConfigKind::Bool => match text {
            "true" | "on" => Ok(ConfigValue::Bool(true)),
            "false" | "off" => Ok(ConfigValue::Bool(false)),
            _ => Err(format!(
                "invalid value '{shown}' for {id}: it is a switch (true or false)"
            )),
        },
        ConfigKind::Int { min, max, .. } => {
            text.parse::<i64>().map(ConfigValue::Int).map_err(|_| {
                format!("invalid value '{shown}' for {id}: a whole number from {min} to {max}")
            })
        }
        ConfigKind::Float { min, max, .. } => match text.parse::<f64>() {
            Ok(f) if f.is_finite() => Ok(ConfigValue::Float(f)),
            _ => Err(format!(
                "invalid value '{shown}' for {id}: a number from {min} to {max}"
            )),
        },
        ConfigKind::Enum { .. } | ConfigKind::String | ConfigKind::Path => {
            Ok(ConfigValue::Str(text.to_string()))
        }
        ConfigKind::List => Ok(ConfigValue::List(
            text.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Human output
// ---------------------------------------------------------------------------

/// The unit of a number row, if any.
fn unit_of(kind: &ConfigKind) -> Option<&str> {
    match kind {
        ConfigKind::Int { unit, .. } | ConfigKind::Float { unit, .. } => unit.as_deref(),
        _ => None,
    }
}

/// One row: `  <id> = <value> [unit]  (<label>; <where>[; read only][; applies ...])`.
pub fn row_text(r: &ConfigRow) -> String {
    let mut value = printable(&value_text(&r.value)).into_owned();
    if value.is_empty() {
        value = "\"\"".into();
    }
    if let Some(u) = unit_of(&r.kind) {
        value = format!("{value} {}", printable(u));
    }
    let mut notes = vec![printable(&r.label).into_owned()];
    notes.push(if r.source.is_empty() {
        "default".to_string()
    } else {
        format!("from {}", printable(&r.source))
    });
    if !r.overridden_by.is_empty() {
        notes.push(format!("overridden by {}", printable(&r.overridden_by)));
    } else if !r.editable {
        notes.push("read only".into());
    } else if !r.enabled && !r.disabled_reason.is_empty() {
        notes.push(printable(&r.disabled_reason).into_owned());
    }
    if r.editable {
        notes.push(
            match r.apply.as_str() {
                obc_ipc::config::APPLY_LIVE => "applies at once",
                obc_ipc::config::APPLY_RESTART => "applies at the next daemon start",
                _ => "applies at the next camera start",
            }
            .into(),
        );
    }
    format!("  {} = {value}  ({})\n", printable(&r.id), notes.join("; "))
}

/// `config show`: each group's label, then its rows.
pub fn schema_text(schema: &ConfigSchema) -> String {
    let mut out = String::new();
    for g in &schema.groups {
        if g.rows.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("{}\n", printable(&g.label)));
        for r in &g.rows {
            out.push_str(&row_text(r));
        }
    }
    out
}

/// `config files`.
pub fn files_text(files: &ConfigFiles) -> String {
    let mut out = format!("Directory: {}\n", printable(&files.dir));
    let width = files.files.iter().map(|f| f.path.len()).max().unwrap_or(0);
    for f in &files.files {
        let mut line = format!("  {:<width$}  {}", printable(&f.path), printable(&f.status));
        if !f.reason.is_empty() {
            line.push_str(&format!(": {}", printable(&f.reason)));
        }
        if f.managed {
            line.push_str(" (written by the daemon)");
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// JSON output
// ---------------------------------------------------------------------------

/// A value as printed.
#[derive(Serialize)]
#[serde(untagged)]
enum JsonValue<'a> {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Cow<'a, str>),
    List(Vec<Cow<'a, str>>),
}

impl<'a> JsonValue<'a> {
    fn of(v: &'a ConfigValue) -> Self {
        match v {
            ConfigValue::Bool(b) => JsonValue::Bool(*b),
            ConfigValue::Int(i) => JsonValue::Int(*i),
            ConfigValue::Float(f) => JsonValue::Float(if f.is_finite() { *f } else { 0.0 }),
            ConfigValue::Str(s) => JsonValue::Str(clip_start(s, MAX_VALUE_BYTES)),
            ConfigValue::List(items) => JsonValue::List(
                items
                    .iter()
                    .take(MAX_LIST_ITEMS)
                    .map(|s| clip_start(s, MAX_VALUE_BYTES))
                    .collect(),
            ),
        }
    }
}

/// A bound of a number row.
#[derive(Serialize)]
#[serde(untagged)]
enum Num {
    Int(i64),
    Float(f64),
}

#[derive(Serialize)]
struct OptionJson<'a> {
    value: Cow<'a, str>,
    label: Cow<'a, str>,
}

/// One row of a `config_group` line, with the keys of its type only.
#[derive(Serialize)]
struct RowJson<'a> {
    id: Cow<'a, str>,
    label: Cow<'a, str>,
    description: Cow<'a, str>,
    #[serde(rename = "type")]
    kind: &'static str,
    value: JsonValue<'a>,
    /// Present (`true`) only when the list value was cut to
    /// [`MAX_LIST_ITEMS`] items.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    value_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    default: Option<JsonValue<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    min: Option<Num>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max: Option<Num>,
    #[serde(skip_serializing_if = "Option::is_none")]
    step: Option<Num>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unit: Option<Cow<'a, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<Vec<OptionJson<'a>>>,
    editable: bool,
    source: Cow<'a, str>,
    overridden_by: Cow<'a, str>,
    apply: Cow<'a, str>,
    enabled: bool,
    disabled_reason: Cow<'a, str>,
}

impl<'a> RowJson<'a> {
    fn of(r: &'a ConfigRow) -> Self {
        let finite = |f: f64| if f.is_finite() { f } else { 0.0 };
        let (min, max, step) = match &r.kind {
            ConfigKind::Int { min, max, step, .. } => (
                Some(Num::Int(*min)),
                Some(Num::Int(*max)),
                Some(Num::Int(*step)),
            ),
            ConfigKind::Float { min, max, step, .. } => (
                Some(Num::Float(finite(*min))),
                Some(Num::Float(finite(*max))),
                Some(Num::Float(finite(*step))),
            ),
            _ => (None, None, None),
        };
        let options = match &r.kind {
            ConfigKind::Enum { options } => Some(
                options
                    .iter()
                    .take(MAX_OPTIONS)
                    .map(|(v, l)| OptionJson {
                        value: clip(v, MAX_VALUE_BYTES),
                        label: clip(l, MAX_LABEL_BYTES),
                    })
                    .collect(),
            ),
            _ => None,
        };
        RowJson {
            id: clip(&r.id, MAX_KEY_BYTES),
            label: clip(&r.label, MAX_LABEL_BYTES),
            description: clip(&r.description, MAX_DESCRIPTION_BYTES),
            kind: r.kind.type_name(),
            value: JsonValue::of(&r.value),
            value_truncated: matches!(&r.value, ConfigValue::List(l) if l.len() > MAX_LIST_ITEMS),
            default: r.default.as_ref().map(JsonValue::of),
            min,
            max,
            step,
            unit: unit_of(&r.kind).map(|u| clip(u, MAX_UNIT_BYTES)),
            options,
            editable: r.editable,
            source: clip_start(&r.source, MAX_PATH_BYTES),
            overridden_by: clip_start(&r.overridden_by, MAX_PATH_BYTES),
            apply: clip(&r.apply, MAX_GROUP_ID_BYTES),
            enabled: r.enabled,
            disabled_reason: clip(&r.disabled_reason, MAX_DESCRIPTION_BYTES),
        }
    }
}

#[derive(Serialize)]
struct GroupJson<'a, 'b> {
    id: Cow<'a, str>,
    label: Cow<'a, str>,
    settings: &'b [RowJson<'a>],
}

#[derive(Serialize)]
struct ConfigGroupLine<'a, 'b> {
    v: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    schema: u32,
    serial: u32,
    index: usize,
    count: usize,
    part: usize,
    parts: usize,
    group: GroupJson<'a, 'b>,
}

fn to_line<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).expect("a line always serializes")
}

/// The config schema as `config_group` lines, packed as `settings_group`
/// lines are: one line per group, or per part of a group that does not fit
/// one (`part` 1 to `parts`), each with at most
/// [`MAX_SETTINGS_PER_LINE`] rows, `index` 0 to `count - 1` over the set:
///
/// `{"v":1,"type":"config_group","schema":1,"serial":S,"index":I,"count":N,"part":P,
/// "parts":Q,"group":{"id":...,"label":...,"settings":[ROW,...]}}`
///
/// A group without rows has no line.
pub fn config_lines(schema: &ConfigSchema) -> Vec<String> {
    let line = |group: &obc_ipc::ConfigGroup,
                rows: &[RowJson<'_>],
                (index, count, part, parts): (usize, usize, usize, usize)| {
        to_line(&ConfigGroupLine {
            v: VERSION,
            kind: "config_group",
            schema: CONFIG_SCHEMA,
            serial: schema.serial,
            index,
            count,
            part,
            parts,
            group: GroupJson {
                id: clip(&group.id, MAX_GROUP_ID_BYTES),
                label: clip(&group.label, MAX_LABEL_BYTES),
                settings: rows,
            },
        })
    };
    let widest = u32::MAX as usize;
    let mut runs = Vec::new();
    for g in &schema.groups {
        let overhead = line(g, &[], (widest, widest, widest, widest)).len();
        let rows: Vec<(RowJson<'_>, usize)> = g
            .rows
            .iter()
            .map(|r| {
                let j = RowJson::of(r);
                let len = to_line(&j).len();
                (j, len)
            })
            .collect();
        let parts = json::pack(rows, overhead, MAX_SETTINGS_PER_LINE);
        if !parts.is_empty() {
            runs.push((g, parts));
        }
    }
    let count: usize = runs.iter().map(|(_, parts)| parts.len()).sum();
    let mut lines = Vec::with_capacity(count);
    for (g, parts) in &runs {
        for (i, rows) in parts.iter().enumerate() {
            lines.push(line(g, rows, (lines.len(), count, i + 1, parts.len())));
        }
    }
    lines
}

#[derive(Serialize)]
struct FileJson<'a> {
    path: Cow<'a, str>,
    status: Cow<'a, str>,
    managed: bool,
    reason: Cow<'a, str>,
}

#[derive(Serialize)]
struct FilesLine<'a> {
    v: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    serial: u32,
    dir: Cow<'a, str>,
    files: Vec<FileJson<'a>>,
    files_truncated: bool,
}

/// `{"v":1,"type":"config_files","serial":S,"dir":"...","files":[{"path","status",
/// "managed","reason"}],"files_truncated":false}`: at most [`MAX_FILES`]
/// files (fewer should the line exceed the cap), `dir` clipped at its
/// start, each `reason` to [`MAX_DESCRIPTION_BYTES`].
pub fn files_line(files: &ConfigFiles) -> String {
    let mut limit = MAX_FILES.min(files.files.len());
    loop {
        let line = to_line(&FilesLine {
            v: VERSION,
            kind: "config_files",
            serial: files.serial,
            dir: clip_start(&files.dir, MAX_PATH_BYTES),
            files: files
                .files
                .iter()
                .take(limit)
                .map(|f| FileJson {
                    path: clip_start(&f.path, MAX_PATH_BYTES),
                    status: clip(&f.status, MAX_GROUP_ID_BYTES),
                    managed: f.managed,
                    reason: clip(&f.reason, MAX_DESCRIPTION_BYTES),
                })
                .collect(),
            files_truncated: files.files.len() > limit,
        });
        if line.len() <= json::MAX_LINE_BYTES || limit == 0 {
            return line;
        }
        limit -= 1;
    }
}

// ---------------------------------------------------------------------------
// Running the commands
// ---------------------------------------------------------------------------

/// The config schema, without starting the daemon.
async fn read_schema() -> Result<ConfigSchema, Failure> {
    let conn = bus::connect().await?;
    let snap = bus::fetch(&conn).await?;
    bus::require_config(snap.interface_revision)?;
    bus::config_schema(&conn).await
}

/// The row `key` of `schema`, or the refusal the daemon would give.
fn find_row<'s>(schema: &'s ConfigSchema, key: &str) -> Result<&'s ConfigRow, Failure> {
    schema.find(key).ok_or_else(|| {
        Failure::Rejected(IpcError::UnknownSetting(format!(
            "No configuration key {key}"
        )))
    })
}

/// A usage error of `config set`, found once the key's type is known.
fn set_usage(msg: String) -> clap::Error {
    use clap::CommandFactory;
    let mut cmd = crate::Cli::command();
    cmd.build();
    match cmd
        .find_subcommand_mut("config")
        .and_then(|c| c.find_subcommand_mut("set"))
    {
        Some(sub) => sub.error(clap::error::ErrorKind::ValueValidation, msg),
        None => cmd.error(clap::error::ErrorKind::ValueValidation, msg),
    }
}

/// What a `config set|reset` run came to.
enum Outcome {
    Done(Box<ConfigRow>),
    Usage(String),
}

/// `config [show|get|set|reset|files]`.
pub async fn run(action: &ConfigAction) -> u8 {
    match action {
        ConfigAction::Show { json } => match within(READ_TIMEOUT, read_schema()).await {
            Ok(schema) => {
                if *json {
                    for line in config_lines(&schema) {
                        out::line(&line);
                    }
                } else {
                    out::print(&schema_text(&schema));
                }
                EXIT_OK
            }
            Err(f) => report(&f, *json),
        },
        ConfigAction::Get { key } => {
            let res = within(READ_TIMEOUT, async {
                let schema = read_schema().await?;
                find_row(&schema, key).map(|r| r.value.clone())
            })
            .await;
            match res {
                Ok(v) => {
                    out::print(&format!("{}\n", printable(&value_text(&v))));
                    EXIT_OK
                }
                Err(f) => report(&f, false),
            }
        }
        ConfigAction::Files { json } => {
            let res = within(READ_TIMEOUT, async {
                let conn = bus::connect().await?;
                let snap = bus::fetch(&conn).await?;
                bus::require_config(snap.interface_revision)?;
                bus::config_files(&conn).await
            })
            .await;
            match res {
                Ok(files) => {
                    if *json {
                        out::line(&files_line(&files));
                    } else {
                        out::print(&files_text(&files));
                    }
                    EXIT_OK
                }
                Err(f) => report(&f, *json),
            }
        }
        ConfigAction::Set { .. } | ConfigAction::Reset { .. } => change(action).await,
    }
}

/// `config set|reset`: the change, then the row read back.
async fn change(action: &ConfigAction) -> u8 {
    let changed = std::cell::Cell::new(false);
    let res = within(CALL_TIMEOUT, async {
        let conn = bus::connect().await?;
        let proxy = bus::proxy(&conn).await?;
        let revision = proxy
            .interface_revision()
            .await
            .map_err(|e| bus::classify(&e, true))?;
        bus::require_config(revision)?;
        let key = match action {
            ConfigAction::Set { key, value } => {
                let schema = proxy.config_schema().await.map_err(bus::classify_ipc)?;
                let row = find_row(&schema, key)?;
                let value = match typed_value(row, value) {
                    Ok(v) => v,
                    Err(msg) => return Ok(Outcome::Usage(msg)),
                };
                proxy
                    .set_config_value(key, &value)
                    .await
                    .map_err(bus::classify_ipc)?;
                key
            }
            ConfigAction::Reset { key } => {
                proxy.reset_config(key).await.map_err(bus::classify_ipc)?;
                key
            }
            _ => unreachable!("not a change"),
        };
        changed.set(true);
        let schema = bus::config_schema(&conn).await?;
        find_row(&schema, key).map(|r| Outcome::Done(Box::new(r.clone())))
    })
    .await;
    match res {
        Ok(Outcome::Done(row)) => {
            out::print(&row_text(&row));
            EXIT_OK
        }
        Ok(Outcome::Usage(msg)) => {
            let _ = set_usage(msg).print();
            EXIT_USAGE
        }
        Err(f) if changed.get() => {
            eprintln!(
                "omarchy-broadcast: the change was made, but reading the key back failed: {}",
                printable(&f.message())
            );
            f.exit_code()
        }
        Err(f) => report(&f, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use obc_ipc::{ConfigFileEntry, ConfigGroup};

    fn row(id: &str, kind: ConfigKind, value: ConfigValue) -> ConfigRow {
        ConfigRow {
            id: id.into(),
            label: "Label".into(),
            description: "Help".into(),
            kind,
            value,
            default: None,
            editable: true,
            source: String::new(),
            overridden_by: String::new(),
            apply: "reload".into(),
            enabled: true,
            disabled_reason: String::new(),
        }
    }

    fn int() -> ConfigKind {
        ConfigKind::Int {
            min: 0,
            max: 60,
            step: 1,
            unit: Some("s".into()),
        }
    }

    #[test]
    fn values_are_typed_by_the_row() {
        let r = row("lifecycle.idle_grace_s", int(), ConfigValue::Int(5));
        assert_eq!(typed_value(&r, "-3"), Ok(ConfigValue::Int(-3)));
        assert!(typed_value(&r, "x").unwrap_err().contains("whole number"));
        let f = row(
            "output.hold_timeout_s",
            ConfigKind::Float {
                min: 0.0,
                max: 60.0,
                step: 0.5,
                unit: None,
            },
            ConfigValue::Float(5.0),
        );
        assert_eq!(typed_value(&f, "2.5"), Ok(ConfigValue::Float(2.5)));
        assert!(typed_value(&f, "nan").is_err());
        let b = row(
            "pipewire.enabled",
            ConfigKind::Bool,
            ConfigValue::Bool(true),
        );
        assert_eq!(typed_value(&b, "off"), Ok(ConfigValue::Bool(false)));
        assert!(typed_value(&b, "maybe").is_err());
        let l = row("modules.dirs", ConfigKind::List, ConfigValue::List(vec![]));
        assert_eq!(
            typed_value(&l, " /a , ,/b"),
            Ok(ConfigValue::List(vec!["/a".into(), "/b".into()]))
        );
        assert_eq!(typed_value(&l, ""), Ok(ConfigValue::List(vec![])));
    }

    #[test]
    fn a_row_says_where_its_value_comes_from() {
        let mut r = row("lifecycle.idle_grace_s", int(), ConfigValue::Int(5));
        assert_eq!(
            row_text(&r),
            "  lifecycle.idle_grace_s = 5 s  (Label; default; applies at the next camera start)\n"
        );
        r.source = "config.d/90-x.toml".into();
        r.overridden_by = "config.d/90-x.toml".into();
        assert_eq!(
            row_text(&r),
            "  lifecycle.idle_grace_s = 5 s  (Label; from config.d/90-x.toml; overridden by \
             config.d/90-x.toml; applies at the next camera start)\n"
        );
        r.editable = false;
        r.overridden_by.clear();
        assert!(row_text(&r).contains("; read only)"));
    }

    #[test]
    fn the_lines_follow_the_contract_and_stay_bounded() {
        let mut rows = vec![
            row("lifecycle.idle_grace_s", int(), ConfigValue::Int(5)),
            row(
                "modules.profile",
                ConfigKind::Enum {
                    options: vec![("quality".into(), "Quality".into())],
                },
                ConfigValue::Str("quality".into()),
            ),
        ];
        rows[0].default = Some(ConfigValue::Int(5));
        let long = "d/".repeat(400);
        for i in 0..20 {
            rows.push(row(
                &format!("modules.x{i}"),
                ConfigKind::List,
                ConfigValue::List(vec![long.clone(); 20]),
            ));
        }
        let schema = ConfigSchema {
            serial: 3,
            groups: vec![ConfigGroup {
                id: "modules".into(),
                label: "Modules".into(),
                rows,
            }],
        };
        let lines = config_lines(&schema);
        assert!(lines.len() > 1);
        for (i, l) in lines.iter().enumerate() {
            assert!(l.len() <= json::MAX_LINE_BYTES, "{}", l.len());
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            assert_eq!(v["type"], "config_group");
            assert_eq!(v["schema"], 1);
            assert_eq!(v["serial"], 3);
            assert_eq!(v["index"], i);
            assert_eq!(v["count"], lines.len());
        }
        let first: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        let r = &first["group"]["settings"][0];
        assert_eq!(r["type"], "int");
        assert_eq!(
            (r["min"].clone(), r["unit"].clone()),
            (0.into(), "s".into())
        );
        assert_eq!(r["default"], 5);
        assert_eq!(
            first["group"]["settings"][1]["options"][0]["value"],
            "quality"
        );
        assert!(first["group"]["settings"][1].get("default").is_none());
        assert!(r.get("value_truncated").is_none(), "only on a cut list");
        let list = &first["group"]["settings"][2];
        assert_eq!(list["value"].as_array().unwrap().len(), 8);
        assert_eq!(list["value_truncated"], true);
        let short = row(
            "modules.order",
            ConfigKind::List,
            ConfigValue::List(vec!["a".into(); 8]),
        );
        let short = serde_json::to_value(RowJson::of(&short)).unwrap();
        assert!(short.get("value_truncated").is_none(), "{short}");

        let files = ConfigFiles {
            serial: 3,
            dir: format!("/{long}"),
            files: (0..20)
                .map(|i| ConfigFileEntry {
                    path: format!("config.d/{i:02}-{}.toml", "x".repeat(200)),
                    status: "error".into(),
                    managed: false,
                    reason: "r".repeat(400),
                })
                .collect(),
        };
        let line = files_line(&files);
        assert!(line.len() <= json::MAX_LINE_BYTES, "{}", line.len());
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["files_truncated"], true);
        assert!(v["dir"].as_str().unwrap().starts_with('…'));
    }
}
