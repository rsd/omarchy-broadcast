//! Human-readable output: `status`, `cameras`, `effect`, the results of
//! `on`, `off`, `camera` and `effect ...`, and the lines of `watch` without
//! `--json`.
//!
//! Every string that comes from the daemon (camera names and ids, the
//! error, the output mode, the effect, the background image) goes through
//! [`printable`] before it reaches the terminal, so a crafted file or camera
//! name cannot send it control sequences or reorder the text around it.

use std::borrow::Cow;
use std::fmt::Write as _;

use obc_ipc::{Snapshot, Stats};

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
    match model::active_camera(s) {
        Some(c) if c.usb.is_empty() => printable(&c.name).into_owned(),
        Some(c) => format!("{} ({})", printable(&c.name), printable(&c.usb)),
        None if s.active_camera.is_empty() => "none".into(),
        None => printable(&s.active_camera).into_owned(),
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

/// `status`: one field per line; the strength with `blur`, the image with
/// `replace`.
pub fn status_text(s: &Snapshot) -> String {
    let mut out = format!(
        "State:      {}\n\
         Enabled:    {}\n\
         Camera:     {}\n\
         Consumers:  {}\n\
         Output:     {}\n\
         Effect:     {}\n",
        s.state,
        yes_no(s.enabled),
        camera_label(s),
        s.consumers,
        printable(&s.output_mode),
        printable(&s.effect),
    );
    match s.effect.as_str() {
        "blur" => out.push_str(&format!("Strength:   {}\n", strength(s.blur_strength))),
        "replace" => out.push_str(&format!("Image:      {}\n", image(&s.background_image))),
        _ => {}
    }
    if !s.error.is_empty() {
        out.push_str(&format!("Error:      {}\n", printable(&s.error)));
    }
    out
}

/// `effect`, and the result of `effect ...`: the effect, the blur strength
/// and the background image, whichever effect is selected.
pub fn effect_text(s: &Snapshot) -> String {
    format!(
        "Effect:     {}\n\
         Strength:   {}\n\
         Image:      {}\n",
        printable(&s.effect),
        strength(s.blur_strength),
        image(&s.background_image),
    )
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
    let mut line = format!(
        "state {} | enabled {} | camera {} | consumers {} | output {} | effect {}",
        s.state,
        yes_no(s.enabled),
        camera_label(s),
        s.consumers,
        printable(&s.output_mode),
        effect_label(s),
    );
    if !s.error.is_empty() {
        line.push_str(&format!(" | error: {}", printable(&s.error)));
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
            "active_camera" => format!("camera: {}", camera_label(s)),
            // Reported with `active_camera` when both change.
            "active_camera_name" if changes.fields.contains(&"active_camera") => continue,
            "active_camera_name" => format!("camera: {}", camera_label(s)),
            "consumers" => format!("consumers: {}", s.consumers),
            "output_mode" => format!("output: {}", printable(&s.output_mode)),
            "effect" => format!("effect: {}", printable(&s.effect)),
            "blur_strength" => format!("blur strength: {}", strength(s.blur_strength)),
            "background_image" => format!("background image: {}", image(&s.background_image)),
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
            interface_revision: 2,
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
        texts.extend(change_lines(&new, &model::diff(&old, &new)));
        new.active_camera = "unlisted\u{1b}[0m".into();
        texts.push(camera_label(&new));
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
        assert_eq!(status_text(&s).lines().count(), 8, "{}", status_text(&s));
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
}
