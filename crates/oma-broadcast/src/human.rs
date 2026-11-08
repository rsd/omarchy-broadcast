//! Human-readable output: `status`, `cameras`, the results of `on`, `off`
//! and `camera`, and the lines of `watch` without `--json`.

use obc_ipc::{Snapshot, Stats};

use crate::model::{self, Changes};

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

/// The active camera as a label: its name and `vid:pid`, its id if it is not
/// listed, or `none`.
pub fn camera_label(s: &Snapshot) -> String {
    match model::active_camera(s) {
        Some(c) if c.usb.is_empty() => c.name.clone(),
        Some(c) => format!("{} ({})", c.name, c.usb),
        None if s.active_camera.is_empty() => "none".into(),
        None => s.active_camera.clone(),
    }
}

/// `status`: one field per line.
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
        s.output_mode,
        s.effect,
    );
    if !s.error.is_empty() {
        out.push_str(&format!("Error:      {}\n", s.error));
    }
    out
}

/// `cameras`: one camera per line in the daemon's order, the active one
/// marked with `*`: name, `vid:pid` (or `-`) and id.
pub fn cameras_text(s: &Snapshot) -> String {
    if s.cameras.is_empty() {
        return "No cameras.\n".into();
    }
    let name_w = s
        .cameras
        .iter()
        .map(|c| c.name.chars().count())
        .max()
        .unwrap_or(0);
    let usb_w = s
        .cameras
        .iter()
        .map(|c| c.usb.chars().count().max(1))
        .max()
        .unwrap_or(1);
    let mut out = String::new();
    for c in &s.cameras {
        let mark = if c.id == s.active_camera { '*' } else { ' ' };
        let usb = if c.usb.is_empty() { "-" } else { &c.usb };
        out.push_str(&format!(
            "{mark} {:<name_w$}  {:<usb_w$}  {}\n",
            c.name, usb, c.id
        ));
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
        s.output_mode,
        s.effect,
    );
    if !s.error.is_empty() {
        line.push_str(&format!(" | error: {}", s.error));
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
            "error" => format!("error: {}", s.error),
            "active_camera" => format!("camera: {}", camera_label(s)),
            // Reported with `active_camera` when both change.
            "active_camera_name" if changes.fields.contains(&"active_camera") => continue,
            "active_camera_name" => format!("camera: {}", camera_label(s)),
            "consumers" => format!("consumers: {}", s.consumers),
            "output_mode" => format!("output: {}", s.output_mode),
            "effect" => format!("effect: {}", s.effect),
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
                    format!("{} (active)", c.name)
                } else {
                    c.name.clone()
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
            consumers: 0,
            interface_revision: 1,
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
