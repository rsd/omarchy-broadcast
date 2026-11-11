//! Human-readable output: `status`, `cameras`, `effect`, `controls`,
//! `settings`, the results of `on`, `off`, `camera`, `effect ...`,
//! `denoise ...`, `controls set|reset` and `settings set|reset`, and the
//! lines of `watch` without `--json`.
//!
//! Every string that comes from the daemon (camera names and ids, the
//! error, the output mode, the effect, the background image, the PipeWire
//! output's state, the names of image controls, the ids, labels, units,
//! option labels and reasons of camera settings) goes through
//! [`printable`] before it reaches the terminal, so a crafted file or camera
//! name cannot send it control sequences or reorder the text around it.

use std::borrow::Cow;
use std::fmt::Write as _;

use obc_ipc::{
    CameraControl, CameraControls, Setting, SettingKind, SettingsTree, Snapshot, State, Stats,
};

use crate::model::{self, Changes};

/// Whether `c` is a bidirectional formatting character: the embeddings and
/// overrides (U+202A to U+202E), the isolates (U+2066 to U+2069) and the
/// implicit marks (U+200E, U+200F, U+061C).
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}' | '\u{061c}')
}

/// `s` made safe to print to a terminal: the C0 and C1 control characters,
/// DEL and the bidirectional formatting characters are written as escapes
/// (`\x1b`, `\u{9b}`, `\u{202e}`), so they show instead of acting. Any
/// other character is printed as it is.
pub fn printable(s: &str) -> Cow<'_, str> {
    if !s.chars().any(|c| c.is_control() || is_bidi_control(c)) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if c.is_control() && c.is_ascii() {
            let _ = write!(out, "\\x{:02x}", u32::from(c));
        } else if c.is_control() || is_bidi_control(c) {
            let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

/// The active camera as a label: its name and `vid:pid`, its id if it is not
/// listed, or `none`.
pub fn camera_label(s: &Snapshot) -> String {
    camera_label_of(s, &s.active_camera)
}

/// The camera `id` as a label, as [`camera_label`] gives the active one.
fn camera_label_of(s: &Snapshot, id: &str) -> String {
    match model::camera(s, id) {
        Some(c) if c.usb.is_empty() => printable(&c.name).into_owned(),
        Some(c) => format!("{} ({})", printable(&c.name), printable(&c.usb)),
        None if id.is_empty() => "none".into(),
        None => printable(id).into_owned(),
    }
}

/// A blur strength as printed: two decimals.
fn strength(value: f64) -> String {
    format!("{value:.2}")
}

/// A background image as printed: the path, or `(none)`.
fn image(path: &str) -> Cow<'_, str> {
    if path.is_empty() {
        Cow::Borrowed("(none)")
    } else {
        printable(path)
    }
}

/// The effect with the option it uses: `blur (strength 0.50)`,
/// `replace (image /path)`, or the bare name.
pub fn effect_label(s: &Snapshot) -> String {
    match s.effect.as_str() {
        "blur" => format!("blur (strength {})", strength(s.blur_strength)),
        "replace" => format!("replace (image {})", image(&s.background_image)),
        other => printable(other).into_owned(),
    }
}

/// Noise removal as printed: `on`, `off`, or `unknown` from a daemon too
/// old to have it.
fn on_off(denoise: Option<bool>) -> &'static str {
    match denoise {
        Some(true) => "on",
        Some(false) => "off",
        None => "unknown",
    }
}

/// The consumers: the total, split into loopback and PipeWire while the
/// PipeWire output is involved (`2 (loopback 1, PipeWire 1)`), and with the
/// preview leases while one is held (`2 (loopback 1, preview 1)`).
pub fn consumers_label(s: &Snapshot) -> String {
    let pipewire = model::pipewire_involved(s);
    if !pipewire && s.preview_consumers == 0 {
        return s.consumers.to_string();
    }
    let mut parts = vec![format!("loopback {}", s.loopback_consumers)];
    if pipewire {
        parts.push(format!("PipeWire {}", s.pipewire_consumers));
    }
    if s.preview_consumers > 0 {
        parts.push(format!("preview {}", s.preview_consumers));
    }
    format!("{} ({})", s.consumers, parts.join(", "))
}

/// `status`: one field per line; the PipeWire output's state while it is
/// involved, the strength with `blur`, the image with `replace`, and noise
/// removal after the effect (unless the daemon is too old to have it).
pub fn status_text(s: &Snapshot) -> String {
    let mut out = format!(
        "State:      {}\n\
         Enabled:    {}\n\
         Camera:     {}\n\
         Consumers:  {}\n\
         Output:     {}\n",
        s.state,
        yes_no(s.enabled),
        camera_label(s),
        consumers_label(s),
        printable(&s.output_mode),
    );
    if model::pipewire_involved(s) {
        out.push_str(&format!("PipeWire:   {}\n", printable(&s.pipewire_output)));
    }
    out.push_str(&format!("Effect:     {}\n", printable(&s.effect)));
    match s.effect.as_str() {
        "blur" => out.push_str(&format!("Strength:   {}\n", strength(s.blur_strength))),
        "replace" => out.push_str(&format!("Image:      {}\n", image(&s.background_image))),
        _ => {}
    }
    if let Some(d) = model::denoise(s) {
        out.push_str(&format!("Denoise:    {}\n", on_off(Some(d))));
    }
    if !s.error.is_empty() {
        out.push_str(&format!("Error:      {}\n", printable(&s.error)));
    }
    if !s.warning.is_empty() {
        out.push_str(&format!("Warning:    {}\n", printable(&s.warning)));
    }
    out
}

/// `effect`, and the result of `effect ...`: the effect, the blur strength
/// and the background image, whichever effect is selected, and noise
/// removal as `status` shows it (unless the daemon is too old to have it).
pub fn effect_text(s: &Snapshot) -> String {
    let mut out = format!(
        "Effect:     {}\n\
         Strength:   {}\n\
         Image:      {}\n",
        printable(&s.effect),
        strength(s.blur_strength),
        image(&s.background_image),
    );
    if let Some(d) = model::denoise(s) {
        out.push_str(&format!("Denoise:    {}\n", on_off(Some(d))));
    }
    out
}

/// After `denoise on|off`.
pub fn denoise_text(s: &Snapshot) -> String {
    format!("Denoise:    {}\n", on_off(model::denoise(s)))
}

/// `cameras`: one camera per line in the daemon's order, the active one
/// marked with `*`: name, `vid:pid` (or `-`) and id.
pub fn cameras_text(s: &Snapshot) -> String {
    if s.cameras.is_empty() {
        return "No cameras.\n".into();
    }
    let rows: Vec<_> = s
        .cameras
        .iter()
        .map(|c| {
            let mark = if c.id == s.active_camera { '*' } else { ' ' };
            let usb = if c.usb.is_empty() {
                Cow::Borrowed("-")
            } else {
                printable(&c.usb)
            };
            (mark, printable(&c.name), usb, printable(&c.id))
        })
        .collect();
    let name_w = rows.iter().map(|r| r.1.chars().count()).max().unwrap_or(0);
    let usb_w = rows.iter().map(|r| r.2.chars().count()).max().unwrap_or(1);
    let mut out = String::new();
    for (mark, name, usb, id) in &rows {
        out.push_str(&format!("{mark} {name:<name_w$}  {usb:<usb_w$}  {id}\n"));
    }
    out
}

/// An image control's range: `min..max`, with the step when it is not 1.
fn control_range(c: &CameraControl) -> String {
    if c.step > 1 {
        format!("{}..{} step {}", c.min, c.max, c.step)
    } else {
        format!("{}..{}", c.min, c.max)
    }
}

/// `controls`, and the result of `controls set|reset`: the camera, one line
/// per image control of `c` (name, value, range, default, and `(stored)`
/// for a value the user set), then whether a change applies now: only
/// while the camera is open (`starting` or `active`).
pub fn controls_text(s: &Snapshot, c: &CameraControls) -> String {
    let mut out = format!("Camera:     {}\n", camera_label_of(s, &c.camera));
    if c.controls.is_empty() {
        out.push_str("No image controls.\n");
    }
    struct Row<'a> {
        name: Cow<'a, str>,
        value: String,
        range: String,
        default: String,
        stored: bool,
    }
    let rows: Vec<Row<'_>> = c
        .controls
        .iter()
        .map(|c| Row {
            name: printable(&c.name),
            value: c.value.to_string(),
            range: control_range(c),
            default: c.default.to_string(),
            stored: c.stored,
        })
        .collect();
    let width = |f: fn(&Row<'_>) -> usize| rows.iter().map(f).max().unwrap_or(0);
    let name_w = width(|r| r.name.chars().count());
    let value_w = width(|r| r.value.len());
    let range_w = width(|r| r.range.len());
    let default_w = width(|r| r.default.len());
    for r in &rows {
        let mark = if r.stored { "  (stored)" } else { "" };
        let line = format!(
            "  {:<name_w$}  {:>value_w$}  {:<range_w$}  default {:<default_w$}{mark}",
            r.name, r.value, r.range, r.default
        );
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out.push_str(applies_text(matches!(
        s.state,
        State::Active | State::Starting
    )));
    out
}

/// Whether a change applies now (`live`) or the next time the camera opens.
fn applies_text(live: bool) -> &'static str {
    if live {
        "Applied live.\n"
    } else {
        "The camera is not in use: changes apply the next time it opens.\n"
    }
}

/// Which settings [`settings_text`] lists.
#[derive(Debug, Clone, Copy)]
pub enum Only<'a> {
    /// Every setting.
    All,
    /// The settings of one group.
    Group(&'a str),
    /// One setting.
    Setting(&'a str),
}

/// A setting's value as printed: an int with its unit, a bool as `on` or
/// `off`, an enum as its option's label (its value id when no option has
/// it), `action` for an action.
fn setting_value(s: &Setting) -> String {
    match &s.kind {
        SettingKind::Bool { value, .. } => on_off(Some(*value)).into(),
        SettingKind::Int { value, unit, .. } => match unit {
            Some(u) if !u.is_empty() => format!("{value} {}", printable(u)),
            _ => value.to_string(),
        },
        SettingKind::Enum { options, value, .. } => {
            let label = options
                .iter()
                .find(|(v, _)| v == value)
                .map_or(value.as_str(), |(_, l)| l.as_str());
            printable(label).into_owned()
        }
        SettingKind::Action | SettingKind::Unknown(_) => "action".into(),
    }
}

/// What `settings set` takes for a setting: `on|off`, `MIN..MAX` (with the
/// step when it is not 1), the option value ids, or nothing for an action.
fn setting_accepts(s: &Setting) -> String {
    match &s.kind {
        SettingKind::Bool { .. } => "on|off".into(),
        SettingKind::Int { min, max, step, .. } if *step > 1 => {
            format!("{min}..{max} step {step}")
        }
        SettingKind::Int { min, max, .. } => format!("{min}..{max}"),
        SettingKind::Enum { options, .. } => options
            .iter()
            .map(|(v, _)| printable(v).into_owned())
            .collect::<Vec<_>>()
            .join("|"),
        SettingKind::Action | SettingKind::Unknown(_) => String::new(),
    }
}

/// One setting as [`settings_text`] prints it.
struct SettingRow<'a> {
    label: Cow<'a, str>,
    value: String,
    id: Cow<'a, str>,
    accepts: String,
    marks: String,
}

/// The groups of one part of [`settings_text`], each under its label, the
/// columns aligned over the part.
fn settings_rows_text(out: &mut String, groups: &[(Cow<'_, str>, Vec<SettingRow<'_>>)]) {
    let all = || groups.iter().flat_map(|(_, rows)| rows.iter());
    let width = |f: fn(&SettingRow<'_>) -> usize| all().map(f).max().unwrap_or(0);
    let label_w = width(|r| r.label.chars().count());
    let value_w = width(|r| r.value.chars().count());
    let id_w = width(|r| r.id.chars().count());
    let accepts_w = width(|r| r.accepts.chars().count());
    for (label, rows) in groups {
        out.push_str(label);
        out.push('\n');
        for r in rows {
            let line = format!(
                "  {:<label_w$}  {:<value_w$}  {:<id_w$}  {:<accepts_w$}{}",
                r.label, r.value, r.id, r.accepts, r.marks
            );
            out.push_str(line.trim_end());
            out.push('\n');
        }
    }
}

/// `settings`, and the result of `settings set|reset`: the camera, then the
/// settings of `tree` (those `only` names) under their group's label, one
/// line each: label, value (with its unit), id, what `settings set` takes,
/// then `(stored)` for a value the user set and, for one that cannot be
/// changed now, the daemon's reason in parentheses; then whether a change
/// applies now. The global settings (revision 8: the video effects, the
/// same for every camera) follow under `All cameras:`; when `only` names
/// global settings alone, the camera part is left out. Settings of a type
/// this client does not know are left out.
pub fn settings_text(s: &Snapshot, tree: &SettingsTree, only: Only<'_>) -> String {
    let mut camera_groups = Vec::new();
    let mut global_groups = Vec::new();
    for g in &tree.groups {
        if matches!(only, Only::Group(id) if id != g.id) {
            continue;
        }
        let rows: Vec<SettingRow<'_>> = g
            .settings
            .iter()
            .filter(|st| !matches!(st.kind, SettingKind::Unknown(_)))
            .filter(|st| !matches!(only, Only::Setting(id) if id != st.id))
            .map(|st| {
                let mut marks = String::new();
                if st.stored {
                    marks.push_str("  (stored)");
                }
                if !st.enabled {
                    let reason = if st.disabled_reason.is_empty() {
                        Cow::Borrowed("cannot be changed now")
                    } else {
                        printable(&st.disabled_reason)
                    };
                    marks.push_str(&format!("  ({reason})"));
                }
                SettingRow {
                    label: printable(&st.label),
                    value: setting_value(st),
                    id: printable(&st.id),
                    accepts: setting_accepts(st),
                    marks,
                }
            })
            .collect();
        if rows.is_empty() {
            continue;
        }
        if g.is_global() {
            global_groups.push((printable(&g.label), rows));
        } else {
            camera_groups.push((printable(&g.label), rows));
        }
    }
    let mut out = String::new();
    let camera_part =
        matches!(only, Only::All) || !camera_groups.is_empty() || global_groups.is_empty();
    if camera_part {
        out.push_str(&format!(
            "Camera:     {}\n",
            camera_label_of(s, &tree.camera)
        ));
        if camera_groups.is_empty() {
            out.push_str(if tree.camera.is_empty() || matches!(only, Only::All) {
                "No camera settings.\n"
            } else {
                "No such camera setting.\n"
            });
        }
        settings_rows_text(&mut out, &camera_groups);
        // Only the active camera is ever open.
        let live = !tree.camera.is_empty()
            && tree.camera == s.active_camera
            && matches!(s.state, State::Active | State::Starting);
        out.push_str(applies_text(live));
    }
    if !global_groups.is_empty() {
        out.push_str("All cameras:\n");
        settings_rows_text(&mut out, &global_groups);
    } else if matches!(only, Only::All)
        && s.interface_revision >= obc_ipc::MODULES_REVISION
        && !tree.groups.iter().any(|g| g.is_global())
    {
        // From revision 9 the effects are the modules': none, none shown.
        out.push_str(&format!("All cameras: {}\n", crate::modules::NO_MODULES));
    }
    out
}

/// `watch`: the image controls of the active camera, after a change.
pub fn controls_change_line(c: &CameraControls) -> String {
    if c.controls.is_empty() {
        return "camera controls: none".into();
    }
    let list: Vec<String> = c
        .controls
        .iter()
        .map(|c| {
            let mark = if c.stored { " (stored)" } else { "" };
            format!("{} {}{mark}", printable(&c.name), c.value)
        })
        .collect();
    format!("camera controls: {}", list.join(", "))
}

/// After `on` / `off`.
pub fn enabled_text(s: &Snapshot) -> String {
    format!(
        "{} (state: {})\n",
        if s.enabled { "Enabled" } else { "Disabled" },
        s.state
    )
}

/// After `camera ...`.
pub fn camera_text(s: &Snapshot) -> String {
    format!("Camera: {}\n", camera_label(s))
}

/// One-line summary, printed by `watch` when it (re)connects.
pub fn summary_line(s: &Snapshot) -> String {
    let pipewire = if model::pipewire_involved(s) {
        format!(" | PipeWire {}", printable(&s.pipewire_output))
    } else {
        String::new()
    };
    let mut line = format!(
        "state {} | enabled {} | camera {} | consumers {} | output {}{pipewire} | effect {}",
        s.state,
        yes_no(s.enabled),
        camera_label(s),
        consumers_label(s),
        printable(&s.output_mode),
        effect_label(s),
    );
    if let Some(d) = model::denoise(s) {
        line.push_str(&format!(" | denoise {}", on_off(Some(d))));
    }
    if !s.error.is_empty() {
        line.push_str(&format!(" | error: {}", printable(&s.error)));
    }
    if !s.warning.is_empty() {
        line.push_str(&format!(" | warning: {}", printable(&s.warning)));
    }
    line
}

/// `watch`: one line per changed field of `s`, then one for the camera list.
pub fn change_lines(s: &Snapshot, changes: &Changes) -> Vec<String> {
    let mut lines = Vec::new();
    for field in &changes.fields {
        let line = match *field {
            "state" => format!("state: {}", s.state),
            "enabled" => format!("enabled: {}", yes_no(s.enabled)),
            "error" if s.error.is_empty() => "error: (cleared)".into(),
            "error" => format!("error: {}", printable(&s.error)),
            "warning" if s.warning.is_empty() => "warning: (cleared)".into(),
            "warning" => format!("warning: {}", printable(&s.warning)),
            "active_camera" => format!("camera: {}", camera_label(s)),
            // Reported with `active_camera` when both change.
            "active_camera_name" if changes.fields.contains(&"active_camera") => continue,
            "active_camera_name" => format!("camera: {}", camera_label(s)),
            "consumers" => format!("consumers: {}", s.consumers),
            // The split is news only while the PipeWire output is involved:
            // without it the loopback's count is the total.
            "loopback_consumers" | "pipewire_consumers" if !model::pipewire_involved(s) => {
                continue;
            }
            "loopback_consumers" => format!("loopback consumers: {}", s.loopback_consumers),
            "pipewire_consumers" => format!("PipeWire consumers: {}", s.pipewire_consumers),
            "preview_consumers" => format!("preview consumers: {}", s.preview_consumers),
            "pipewire_output" => format!("PipeWire: {}", printable(&s.pipewire_output)),
            "output_mode" => format!("output: {}", printable(&s.output_mode)),
            "effect" => format!("effect: {}", printable(&s.effect)),
            "blur_strength" => format!("blur strength: {}", strength(s.blur_strength)),
            "background_image" => format!("background image: {}", image(&s.background_image)),
            "denoise" => format!("denoise: {}", on_off(model::denoise(s))),
            "interface_revision" => format!("interface revision: {}", s.interface_revision),
            other => format!("{other}: changed"),
        };
        lines.push(line);
    }
    if changes.cameras {
        let names: Vec<String> = s
            .cameras
            .iter()
            .map(|c| {
                if c.id == s.active_camera {
                    format!("{} (active)", printable(&c.name))
                } else {
                    printable(&c.name).into_owned()
                }
            })
            .collect();
        lines.push(if names.is_empty() {
            "cameras: none".into()
        } else {
            format!("cameras: {}", names.join(", "))
        });
    }
    if let (true, Some(c)) = (changes.controls, &s.camera_controls) {
        lines.push(controls_change_line(c));
    }
    lines
}

fn measured(value: f64, unit: &str) -> String {
    if value.is_finite() && Stats::is_measured(value) {
        format!("{value:.1}{unit}")
    } else {
        "n/a".into()
    }
}

/// `watch`: one `Stats` signal.
pub fn stats_text(st: &Stats) -> String {
    format!(
        "stats: {} fps | latency {} | cpu {} | gpu {}",
        measured(st.fps, ""),
        measured(st.latency_p50_ms, " ms"),
        measured(st.cpu_percent, "%"),
        measured(st.gpu_percent, "%"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use obc_ipc::{CameraInfo, State};

    fn sample() -> Snapshot {
        Snapshot {
            state: State::Idle,
            error: String::new(),
            warning: String::new(),
            enabled: true,
            cameras: vec![
                CameraInfo {
                    id: "usb-1234_abcd".into(),
                    name: "First Camera".into(),
                    usb: "1234:abcd".into(),
                },
                CameraInfo {
                    id: "platform-video2".into(),
                    name: "Second".into(),
                    usb: String::new(),
                },
            ],
            active_camera: "usb-1234_abcd".into(),
            output_mode: "NV12 1920x1080@30".into(),
            effect: "none".into(),
            blur_strength: 0.5,
            background_image: String::new(),
            consumers: 0,
            loopback_consumers: 0,
            pipewire_consumers: 0,
            preview_consumers: 0,
            pipewire_output: "off".into(),
            // Before noise removal: the outputs without it.
            denoise: false,
            camera_controls: None,
            settings_revision: None,
            settings_schema_serial: None,
            modules_serial: None,
            backend_profile: None,
            config_serial: None,
            interface_revision: 3,
        }
    }

    #[test]
    fn status_lists_the_fields() {
        let s = sample();
        assert_eq!(
            status_text(&s),
            "State:      idle\n\
             Enabled:    yes\n\
             Camera:     First Camera (1234:abcd)\n\
             Consumers:  0\n\
             Output:     NV12 1920x1080@30\n\
             Effect:     none\n"
        );
        let mut e = s.clone();
        e.error = "no camera".into();
        e.active_camera.clear();
        let text = status_text(&e);
        assert!(text.ends_with("Error:      no camera\n"), "{text}");
        let mut w = s.clone();
        w.warning = "effect blur unavailable: x".into();
        let warned = status_text(&w);
        assert!(
            warned.ends_with("Effect:     none\nWarning:    effect blur unavailable: x\n"),
            "{warned}"
        );
        assert!(
            summary_line(&w).ends_with(" | warning: effect blur unavailable: x"),
            "{}",
            summary_line(&w)
        );
        let mut cleared = w.clone();
        cleared.warning.clear();
        assert_eq!(
            change_lines(&cleared, &model::diff(&w, &cleared)),
            ["warning: (cleared)"]
        );
        assert_eq!(
            change_lines(&w, &model::diff(&s, &w)),
            ["warning: effect blur unavailable: x"]
        );
        assert!(text.contains("Camera:     none\n"), "{text}");
        e.active_camera = "unlisted".into();
        assert_eq!(camera_label(&e), "unlisted");

        let mut b = s.clone();
        b.effect = "blur".into();
        b.blur_strength = 0.8;
        assert!(
            status_text(&b).ends_with("Effect:     blur\nStrength:   0.80\n"),
            "{}",
            status_text(&b)
        );
        b.effect = "replace".into();
        b.background_image = "/home/u/bg.png".into();
        b.error = "boom".into();
        assert!(
            status_text(&b)
                .ends_with("Effect:     replace\nImage:      /home/u/bg.png\nError:      boom\n"),
            "{}",
            status_text(&b)
        );
    }

    #[test]
    fn the_consumers_show_the_preview_leases_while_one_is_held() {
        let mut s = sample();
        s.consumers = 2;
        s.loopback_consumers = 1;
        s.preview_consumers = 1;
        assert_eq!(consumers_label(&s), "2 (loopback 1, preview 1)");
        s.pipewire_output = "ready".into();
        s.consumers = 1;
        s.loopback_consumers = 0;
        assert_eq!(consumers_label(&s), "1 (loopback 0, PipeWire 0, preview 1)");
        s.preview_consumers = 0;
        s.pipewire_output = "off".into();
        s.consumers = 0;
        assert_eq!(consumers_label(&s), "0");
        let changes = Changes {
            fields: vec!["preview_consumers"],
            cameras: false,
            controls: false,
        };
        assert_eq!(change_lines(&s, &changes), ["preview consumers: 0"]);
    }

    #[test]
    fn status_splits_the_consumers_while_pipewire_is_involved() {
        let mut s = sample();
        s.consumers = 2;
        s.loopback_consumers = 1;
        s.pipewire_consumers = 1;
        s.pipewire_output = "streaming".into();
        assert_eq!(
            status_text(&s),
            "State:      idle\n\
             Enabled:    yes\n\
             Camera:     First Camera (1234:abcd)\n\
             Consumers:  2 (loopback 1, PipeWire 1)\n\
             Output:     NV12 1920x1080@30\n\
             PipeWire:   streaming\n\
             Effect:     none\n"
        );
        assert_eq!(
            summary_line(&s),
            "state idle | enabled yes | camera First Camera (1234:abcd) \
             | consumers 2 (loopback 1, PipeWire 1) | output NV12 1920x1080@30 \
             | PipeWire streaming | effect none"
        );
        // Configured, unavailable: shown, nobody linked.
        s.consumers = 1;
        s.pipewire_consumers = 0;
        s.pipewire_output = "unsupported".into();
        let text = status_text(&s);
        assert!(
            text.contains("Consumers:  1 (loopback 1, PipeWire 0)\nOutput:     NV12 1920x1080@30\nPipeWire:   unsupported\n"),
            "{text}"
        );
        // Not configured (or a daemon without it): no split, no line.
        s.pipewire_output = "off".into();
        let text = status_text(&s);
        assert!(text.contains("Consumers:  1\n"), "{text}");
        assert!(!text.contains("PipeWire"), "{text}");
        assert!(!summary_line(&s).contains("PipeWire"));
    }

    #[test]
    fn watch_lines_of_the_pipewire_fields() {
        let mut old = sample();
        old.pipewire_output = "ready".into();
        let mut new = old.clone();
        new.consumers = 1;
        new.pipewire_consumers = 1;
        new.pipewire_output = "streaming".into();
        assert_eq!(
            change_lines(&new, &model::diff(&old, &new)),
            vec![
                "consumers: 1",
                "PipeWire consumers: 1",
                "PipeWire: streaming"
            ]
        );
        let mut more = new.clone();
        more.consumers = 2;
        more.loopback_consumers = 1;
        assert_eq!(
            change_lines(&more, &model::diff(&new, &more)),
            vec!["consumers: 2", "loopback consumers: 1"]
        );
        // Without the PipeWire output the loopback count is the total: its
        // own change is not repeated.
        let old = sample();
        let mut new = old.clone();
        new.consumers = 1;
        new.loopback_consumers = 1;
        assert_eq!(
            change_lines(&new, &model::diff(&old, &new)),
            vec!["consumers: 1"]
        );
        // The output going off is reported.
        let mut on = sample();
        on.pipewire_output = "failed".into();
        let off = sample();
        assert_eq!(
            change_lines(&off, &model::diff(&on, &off)),
            vec!["PipeWire: off"]
        );
    }

    #[test]
    fn denoise_is_shown_next_to_the_effect_from_revision_4() {
        let mut s = sample();
        assert!(!status_text(&s).contains("Denoise"), "{}", status_text(&s));
        assert!(!summary_line(&s).contains("denoise"));
        assert_eq!(denoise_text(&s), "Denoise:    unknown\n");
        s.interface_revision = 4;
        s.denoise = true;
        s.effect = "blur".into();
        s.error = "noise removal unavailable: x".into();
        assert!(
            status_text(&s).ends_with(
                "Effect:     blur\nStrength:   0.50\nDenoise:    on\n\
                 Error:      noise removal unavailable: x\n"
            ),
            "{}",
            status_text(&s)
        );
        assert!(
            summary_line(&s).ends_with(
                "| effect blur (strength 0.50) | denoise on | error: noise removal unavailable: x"
            ),
            "{}",
            summary_line(&s)
        );
        assert_eq!(denoise_text(&s), "Denoise:    on\n");
        let mut off = s.clone();
        off.denoise = false;
        assert!(status_text(&off).contains("Denoise:    off\n"));
        assert_eq!(denoise_text(&off), "Denoise:    off\n");
        assert_eq!(
            change_lines(&off, &model::diff(&s, &off)),
            vec!["denoise: off"]
        );
        // A daemon of revision 4 replacing an older one: the value appears.
        let old = sample();
        let mut new = old.clone();
        new.interface_revision = 4;
        assert_eq!(
            change_lines(&new, &model::diff(&old, &new)),
            vec!["denoise: off", "interface revision: 4"]
        );
    }

    #[test]
    fn effect_lists_effect_strength_and_image() {
        let mut s = sample();
        assert_eq!(
            effect_text(&s),
            "Effect:     none\nStrength:   0.50\nImage:      (none)\n"
        );
        assert_eq!(effect_label(&s), "none");
        s.effect = "replace".into();
        s.background_image = "/home/u/bg.png".into();
        assert_eq!(
            effect_text(&s),
            "Effect:     replace\nStrength:   0.50\nImage:      /home/u/bg.png\n"
        );
        assert_eq!(effect_label(&s), "replace (image /home/u/bg.png)");
        s.effect = "blur".into();
        s.blur_strength = 1.0;
        assert_eq!(effect_label(&s), "blur (strength 1.00)");
        // From revision 4, noise removal follows, as in `status`.
        s.interface_revision = 4;
        s.denoise = true;
        assert_eq!(
            effect_text(&s),
            "Effect:     blur\nStrength:   1.00\nImage:      /home/u/bg.png\nDenoise:    on\n"
        );
    }

    fn controls() -> CameraControls {
        let control = |name: &str, min, max, step, default, value, stored| CameraControl {
            name: String::from(name),
            min,
            max,
            step,
            default,
            value,
            stored,
        };
        CameraControls {
            camera: "usb-1234_abcd".into(),
            controls: vec![
                control("brightness", -64, 64, 1, 0, -10, true),
                control("saturation", 0, 100, 2, 50, 50, false),
                control("sharpness", 0, 255, 1, 128, 200, true),
            ],
        }
    }

    #[test]
    fn controls_list_values_ranges_defaults_and_where_a_change_applies() {
        let mut s = sample();
        let c = controls();
        assert_eq!(
            controls_text(&s, &c),
            "Camera:     First Camera (1234:abcd)\n  \
             brightness  -10  -64..64        default 0    (stored)\n  \
             saturation   50  0..100 step 2  default 50\n  \
             sharpness   200  0..255         default 128  (stored)\n\
             The camera is not in use: changes apply the next time it opens.\n"
        );
        // The camera is open from `starting` on: a change reaches it then.
        for state in [State::Starting, State::Active] {
            s.state = state;
            assert!(
                controls_text(&s, &c).ends_with("(stored)\nApplied live.\n"),
                "{}",
                controls_text(&s, &c)
            );
        }
        // An unlisted camera by its id; no control at all.
        let none = CameraControls {
            camera: "platform-video9".into(),
            controls: Vec::new(),
        };
        assert_eq!(
            controls_text(&s, &none),
            "Camera:     platform-video9\nNo image controls.\nApplied live.\n"
        );
        // `watch`: one line for a change.
        assert_eq!(
            controls_change_line(&c),
            "camera controls: brightness -10 (stored), saturation 50, sharpness 200 (stored)"
        );
        assert_eq!(controls_change_line(&none), "camera controls: none");
        let old = s.clone();
        let mut new = s.clone();
        new.camera_controls = Some(c.clone());
        assert_eq!(
            change_lines(&new, &model::diff(&old, &new)),
            vec![controls_change_line(&c)]
        );
        assert!(change_lines(&new, &model::diff(&new, &new)).is_empty());
    }

    #[test]
    fn printable_escapes_control_and_bidi_characters() {
        assert!(matches!(printable("/home/u/bg é 🎥.png"), Cow::Borrowed(_)));
        // ESC sequences (CSI, OSC 52 ended by BEL), C1 CSI, newline, DEL.
        assert_eq!(
            printable("a\u{1b}[2Jb\u{1b}]52;c;eA==\u{7}"),
            "a\\x1b[2Jb\\x1b]52;c;eA==\\x07"
        );
        assert_eq!(printable("x\u{9b}31my"), "x\\u{9b}31my");
        assert_eq!(
            printable("one\ntwo\r\t\u{7f}"),
            "one\\x0atwo\\x0d\\x09\\x7f"
        );
        // Bidi overrides, isolates and marks.
        assert_eq!(printable("/u/\u{202e}gnp.exe"), "/u/\\u{202e}gnp.exe");
        assert_eq!(
            printable("\u{2066}\u{2069}\u{200f}\u{61c}"),
            "\\u{2066}\\u{2069}\\u{200f}\\u{61c}"
        );
    }

    #[test]
    fn daemon_strings_are_printable_in_every_output() {
        let evil = "\u{1b}]0;pwned\u{7}\u{9b}2J\n\u{202e}";
        let mut s = sample();
        s.cameras[0].name = format!("Cam{evil}");
        s.cameras[0].usb = format!("12{evil}");
        s.cameras[1].id = format!("id{evil}");
        s.output_mode = format!("mode{evil}");
        s.error = format!("error{evil}");
        s.background_image = format!("/bg{evil}.png");
        s.pipewire_output = format!("ready{evil}");
        let mut texts = vec![
            status_text(&s),
            cameras_text(&s),
            camera_text(&s),
            effect_text(&s),
            summary_line(&s),
        ];
        s.effect = format!("replace{evil}");
        texts.push(effect_text(&s));
        texts.push(effect_label(&s));
        s.effect = "replace".into();
        texts.push(status_text(&s));
        texts.push(summary_line(&s));
        let old = sample();
        let mut new = s.clone();
        new.active_camera = s.cameras[1].id.clone();
        new.effect = format!("blur{evil}");
        new.pipewire_consumers = 1;
        texts.extend(change_lines(&new, &model::diff(&old, &new)));
        new.active_camera = "unlisted\u{1b}[0m".into();
        texts.push(camera_label(&new));
        let mut c = controls();
        c.controls[0].name = format!("bright{evil}");
        c.camera = format!("cam{evil}");
        texts.push(controls_text(&s, &c));
        texts.push(controls_change_line(&c));
        new.camera_controls = Some(c);
        texts.extend(change_lines(&new, &model::diff(&old, &new)));
        for text in &texts {
            // Only the line ends this module writes itself.
            let body = text.trim_end_matches('\n');
            for line in body.split('\n') {
                assert!(
                    !line.chars().any(|c| c.is_control() || is_bidi_control(c)),
                    "{text:?}"
                );
            }
        }
        // Each field is still one line.
        assert_eq!(status_text(&s).lines().count(), 9, "{}", status_text(&s));
        assert!(status_text(&s).contains("PipeWire:   ready\\x1b]0;pwned"));
        assert!(status_text(&s).contains("Image:      /bg\\x1b]0;pwned"));
        assert_eq!(cameras_text(&s).lines().count(), 2);
    }

    #[test]
    fn cameras_mark_the_active_one() {
        assert_eq!(
            cameras_text(&sample()),
            "* First Camera  1234:abcd  usb-1234_abcd\n  \
             Second        -          platform-video2\n"
        );
        let mut s = sample();
        s.cameras.clear();
        assert_eq!(cameras_text(&s), "No cameras.\n");
    }

    #[test]
    fn command_results() {
        let mut s = sample();
        assert_eq!(enabled_text(&s), "Enabled (state: idle)\n");
        s.enabled = false;
        s.state = State::Off;
        assert_eq!(enabled_text(&s), "Disabled (state: off)\n");
        s.active_camera = "platform-video2".into();
        assert_eq!(camera_text(&s), "Camera: Second\n");
    }

    #[test]
    fn watch_lines() {
        let s = sample();
        assert_eq!(
            summary_line(&s),
            "state idle | enabled yes | camera First Camera (1234:abcd) | consumers 0 \
             | output NV12 1920x1080@30 | effect none"
        );
        let mut new = s.clone();
        new.state = State::Active;
        new.consumers = 1;
        new.active_camera = "platform-video2".into();
        new.error = "boom".into();
        let changes = model::diff(&s, &new);
        assert_eq!(
            change_lines(&new, &changes),
            vec![
                "state: active",
                "error: boom",
                "camera: Second",
                "consumers: 1"
            ]
        );
        let mut fewer = new.clone();
        fewer.cameras.truncate(1);
        fewer.error.clear();
        let changes = model::diff(&new, &fewer);
        assert_eq!(
            change_lines(&fewer, &changes),
            vec![
                "error: (cleared)",
                "camera: platform-video2",
                "cameras: First Camera"
            ]
        );
        let mut fx = s.clone();
        fx.effect = "blur".into();
        fx.blur_strength = 0.25;
        fx.background_image = "/bg.png".into();
        let changes = model::diff(&s, &fx);
        assert_eq!(
            change_lines(&fx, &changes),
            vec![
                "effect: blur",
                "blur strength: 0.25",
                "background image: /bg.png"
            ]
        );
        assert!(summary_line(&fx).ends_with("| effect blur (strength 0.25)"));
        assert_eq!(
            stats_text(&Stats {
                fps: 30.0,
                latency_p50_ms: 12.5,
                cpu_percent: 3.0,
                gpu_percent: -1.0,
            }),
            "stats: 30.0 fps | latency 12.5 ms | cpu 3.0% | gpu n/a"
        );
    }

    fn settings_tree() -> SettingsTree {
        use obc_ipc::{Condition, ConditionOp, SettingValue, SettingsGroup};
        let setting = |id: &str, label: &str, kind| Setting {
            id: id.into(),
            label: label.into(),
            kind,
            scope: "camera".into(),
            stored: false,
            enabled: true,
            disabled_reason: String::new(),
            enabled_when: Vec::new(),
            hint: None,
        };
        let mut time = setting(
            "camera.exposure.exposure_time_absolute",
            "Exposure time",
            SettingKind::Int {
                min: 3,
                max: 2047,
                step: 1,
                default: 156,
                value: 1250,
                unit: Some("\u{d7}100 \u{b5}s".into()),
            },
        );
        time.enabled = false;
        time.disabled_reason = "Set exposure to manual to change this".into();
        time.enabled_when = vec![Condition {
            id: "camera.exposure.auto_exposure".into(),
            op: ConditionOp::In,
            value: SettingValue::List(vec![SettingValue::Str("manual".into())]),
        }];
        let mut keep = setting(
            "camera.exposure.keep_framerate",
            "Keep 30 fps in low light",
            SettingKind::Bool {
                default: true,
                value: false,
            },
        );
        keep.stored = true;
        SettingsTree {
            revision: 4,
            serial: 1,
            camera: "usb-1234_abcd".into(),
            groups: vec![
                SettingsGroup {
                    id: "image".into(),
                    label: "Image".into(),
                    settings: vec![setting(
                        "camera.image.sharpness",
                        "Sharpness",
                        SettingKind::Int {
                            min: 0,
                            max: 255,
                            step: 1,
                            default: 128,
                            value: 128,
                            unit: None,
                        },
                    )],
                },
                SettingsGroup {
                    id: "exposure".into(),
                    label: "Exposure".into(),
                    settings: vec![
                        setting(
                            "camera.exposure.auto_exposure",
                            "Exposure mode",
                            SettingKind::Enum {
                                options: vec![
                                    ("manual".into(), "Manual".into()),
                                    ("aperture_priority".into(), "Auto".into()),
                                ],
                                default: "aperture_priority".into(),
                                value: "aperture_priority".into(),
                            },
                        ),
                        time,
                        keep,
                        setting(
                            "camera.exposure.future",
                            "Unknown \u{1b}[2J",
                            SettingKind::Unknown("colour".into()),
                        ),
                    ],
                },
                SettingsGroup {
                    id: "focus".into(),
                    label: "Focus & \u{202e}zoom".into(),
                    settings: vec![setting(
                        "camera.focus.pan_reset",
                        "Recentre pan",
                        SettingKind::Action,
                    )],
                },
            ],
        }
    }

    #[test]
    fn settings_list_by_group_with_values_ids_and_reasons() {
        let mut s = sample();
        let tree = settings_tree();
        assert_eq!(
            settings_text(&s, &tree, Only::All),
            "Camera:     First Camera (1234:abcd)\n\
             Image\n  \
             Sharpness                 128           camera.image.sharpness                  0..255\n\
             Exposure\n  \
             Exposure mode             Auto          camera.exposure.auto_exposure           manual|aperture_priority\n  \
             Exposure time             1250 \u{d7}100 \u{b5}s  camera.exposure.exposure_time_absolute  3..2047                   (Set exposure to manual to change this)\n  \
             Keep 30 fps in low light  off           camera.exposure.keep_framerate          on|off                    (stored)\n\
             Focus & \\u{202e}zoom\n  \
             Recentre pan              action        camera.focus.pan_reset\n\
             The camera is not in use: changes apply the next time it opens.\n"
        );
        // One setting, one group; open: live.
        s.state = State::Active;
        assert_eq!(
            settings_text(&s, &tree, Only::Setting("camera.exposure.keep_framerate")),
            "Camera:     First Camera (1234:abcd)\n\
             Exposure\n  \
             Keep 30 fps in low light  off  camera.exposure.keep_framerate  on|off  (stored)\n\
             Applied live.\n"
        );
        assert_eq!(
            settings_text(&s, &tree, Only::Group("image")),
            "Camera:     First Camera (1234:abcd)\n\
             Image\n  \
             Sharpness  128  camera.image.sharpness  0..255\n\
             Applied live.\n"
        );
        // Another camera than the active one is not open.
        let mut other = tree.clone();
        other.camera = "platform-video2".into();
        assert!(
            settings_text(&s, &other, Only::Group("image")).starts_with("Camera:     Second\n")
        );
        assert!(
            settings_text(&s, &other, Only::All)
                .ends_with("The camera is not in use: changes apply the next time it opens.\n")
        );
        // No camera.
        let none = SettingsTree::default();
        assert_eq!(
            settings_text(&s, &none, Only::All),
            "Camera:     none\nNo camera settings.\n\
             The camera is not in use: changes apply the next time it opens.\n"
        );
    }

    /// The global groups of a revision-8 daemon.
    fn global_groups() -> Vec<obc_ipc::SettingsGroup> {
        let setting = |id: &str, label: &str, kind| Setting {
            id: id.into(),
            label: label.into(),
            kind,
            scope: "global".into(),
            stored: false,
            enabled: true,
            disabled_reason: String::new(),
            enabled_when: Vec::new(),
            hint: None,
        };
        let mode = setting(
            "effect.background.mode",
            "Effect",
            SettingKind::Enum {
                options: vec![
                    ("none".into(), "Off".into()),
                    ("blur".into(), "Blur".into()),
                    ("replace".into(), "Replace".into()),
                ],
                default: "none".into(),
                value: "none".into(),
            },
        );
        let mut blur = setting(
            "effect.background.blur_strength",
            "Blur strength",
            SettingKind::Int {
                min: 0,
                max: 100,
                step: 1,
                default: 50,
                value: 50,
                unit: Some("%".into()),
            },
        );
        blur.enabled = false;
        blur.disabled_reason = "Choose Blur to change this".into();
        let mut denoise = setting(
            "effect.video.denoise",
            "Noise removal",
            SettingKind::Bool {
                default: false,
                value: false,
            },
        );
        denoise.stored = true;
        denoise.enabled = false;
        denoise.disabled_reason = "Not installed".into();
        vec![
            obc_ipc::SettingsGroup {
                id: "background".into(),
                label: "Background effect".into(),
                settings: vec![mode, blur],
            },
            obc_ipc::SettingsGroup {
                id: "video".into(),
                label: "Video".into(),
                settings: vec![denoise],
            },
        ]
    }

    const GLOBAL_TEXT: &str = "All cameras:\n\
         Background effect\n  \
         Effect         Off   effect.background.mode           none|blur|replace\n  \
         Blur strength  50 %  effect.background.blur_strength  0..100             (Choose Blur to change this)\n\
         Video\n  \
         Noise removal  off   effect.video.denoise             on|off             (stored)  (Not installed)\n";

    #[test]
    fn global_settings_follow_the_cameras_under_their_own_heading() {
        let mut s = sample();
        s.state = State::Active;
        let mut tree = settings_tree();
        tree.groups.truncate(1);
        tree.groups.extend(global_groups());
        assert_eq!(
            settings_text(&s, &tree, Only::All),
            format!(
                "Camera:     First Camera (1234:abcd)\n\
                 Image\n  \
                 Sharpness  128  camera.image.sharpness  0..255\n\
                 Applied live.\n{GLOBAL_TEXT}"
            )
        );
        // A global setting or group alone: no camera part.
        assert_eq!(
            settings_text(&s, &tree, Only::Setting("effect.video.denoise")),
            "All cameras:\nVideo\n  \
             Noise removal  off  effect.video.denoise  on|off  (stored)  (Not installed)\n"
        );
        assert!(
            settings_text(&s, &tree, Only::Group("background"))
                .starts_with("All cameras:\nBackground effect\n")
        );
        // A camera setting alone: no global part.
        assert!(!settings_text(&s, &tree, Only::Group("image")).contains("All cameras"));
        // No camera: the global settings all the same.
        let none = SettingsTree {
            groups: global_groups(),
            ..SettingsTree::default()
        };
        assert_eq!(
            settings_text(&s, &none, Only::All),
            format!(
                "Camera:     none\nNo camera settings.\n\
                 The camera is not in use: changes apply the next time it opens.\n{GLOBAL_TEXT}"
            )
        );
    }
}
