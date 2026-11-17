//! The JSON lines printed by `status --json`, `cameras --json`,
//! `controls --json`, `settings --json`, `watch --json` and `preview --json`.
//! The contract is documented in `docs/cli-json.md`; this module is its only
//! producer.
//!
//! Every line is one JSON object carrying `"v"` ([`VERSION`]) and `"type"`,
//! and is at most [`MAX_LINE_BYTES`] bytes long: strings are clipped to the
//! bounds below (measured in bytes of their JSON encoding, so escapes count)
//! with a trailing [`ELLIPSIS`] (a path, `background_image`, keeps its end,
//! the file name, behind a leading one), the camera list is cut at
//! [`MAX_CAMERAS`] entries and a list of image controls at [`MAX_CONTROLS`]
//! (a `snapshot` line cuts its controls further, then its cameras, should
//! the line still exceed the cap). The `path` of `preview` and `frame` lines is
//! the one exception: it is never clipped (a client opens that file), and a
//! path over [`MAX_PREVIEW_PATH_BYTES`] is refused instead
//! ([`preview_path_fits`], [`CODE_PREVIEW_PATH_TOO_LONG`]).
//!
//! The camera settings (revision 7) are a *set* of `settings_group` lines
//! ([`settings_lines`]): each carries one group, or a part of one, with at
//! most [`MAX_SETTINGS_PER_LINE`] settings, packed greedily by encoded
//! length so that a line stays under the cap; every string of a setting is
//! clipped to its bound below, a setting lists at most [`MAX_OPTIONS`]
//! options and [`MAX_CONDITIONS`] conditions, and a setting of a type this
//! client does not know is left out. A change is a `settings_delta` line
//! ([`settings_delta_lines`], at most [`MAX_CHANGES_PER_LINE`] changes,
//! packed the same way), and nothing to show (no camera and no global
//! setting, or no setting this CLI can show) a `settings_clear` line. The
//! global settings of a revision-8 daemon (the video effects) are groups
//! like the others, after the camera's, and are shown with no camera too;
//! every setting object carries its `scope` (`camera` or `global`, clipped
//! to [`MAX_SCOPE_BYTES`]), which a client routes by.
//!
//! The camera's health (revision 20) is the `camera_health` of a `snapshot`
//! line (`null` before revision 20) and the body of a `camera_health` line
//! ([`camera_health_line`]): its lists are cut at [`MAX_HEALTH_WARNINGS`],
//! [`MAX_SUGGESTIONS`], [`MAX_RECOMMENDED`] and [`MAX_HEALTH_STATUS`], and
//! further (the recommended changes first, then the suggestions, then the
//! warnings, then the status lines) should a line still exceed the cap; a
//! `snapshot` line cuts them after its image controls and before its
//! cameras. From revision 26 it also carries the camera's `shutter` and
//! `show_mode` (`""` when unknown) and its `status` lines; an older
//! daemon's read as `""` and `[]`.

use std::borrow::Cow;

use obc_ipc::{
    CameraControl, CameraControls, CameraHealth, CameraInfo, Condition, MicStatus, SETTINGS_SCHEMA,
    Setting, SettingChange, SettingKind, SettingValue, SettingsGroup, SettingsTree, Snapshot,
    Stats,
};
use serde::Serialize;

use crate::model;

/// Schema version carried by every line as `"v"`. Raised only on a breaking
/// change; added fields and line types are not breaking.
pub const VERSION: u32 = 1;
/// No line, without its newline, is longer than this many bytes.
pub const MAX_LINE_BYTES: usize = 4096;
/// At most this many cameras are listed; `cameras_truncated` says whether
/// more were present.
pub const MAX_CAMERAS: usize = 12;
/// Bound of the `error` text.
pub const MAX_ERROR_BYTES: usize = 512;
/// Bound of the `warning` text: as `error`'s, so a module's reason with
/// its hint at the end (an effect failure, [`MAX_ERROR_BYTES`] before
/// revision 11) is never cut.
pub const MAX_WARNING_BYTES: usize = 512;
/// Bound of a camera id (`id`, `active_camera`).
pub const MAX_ID_BYTES: usize = 96;
/// At most this many `stage_values` are listed (revision 16).
pub const MAX_STAGE_VALUES: usize = 8;
/// Bound of a stage value's `stage` and `name`.
pub const MAX_STAGE_VALUE_WORD_BYTES: usize = 48;
/// Bound of a camera name (`name`, `active_camera_name`).
pub const MAX_NAME_BYTES: usize = 64;
/// Bound of a USB `vid:pid` (`usb`).
pub const MAX_USB_BYTES: usize = 16;
/// Bound of `output_mode`.
pub const MAX_MODE_BYTES: usize = 64;
/// Bound of `effect`.
pub const MAX_EFFECT_BYTES: usize = 32;
/// Bound of `pipewire_output`.
pub const MAX_PIPEWIRE_OUTPUT_BYTES: usize = 32;
/// The longest `mic.state`, in bytes.
pub const MAX_MIC_STATE_BYTES: usize = 32;
/// The longest `mic.provider`, in bytes.
pub const MAX_MIC_PROVIDER_BYTES: usize = 256;
/// The longest `mic.error`, in bytes.
pub const MAX_MIC_ERROR_BYTES: usize = 256;
/// Bound of `background_image`. A longer path is clipped at its start
/// ([`clip_start`]), so the file name survives. With every string and
/// number at its worst and no image controls, a `snapshot` line holds
/// [`MAX_CAMERAS`] − 6 cameras under [`MAX_LINE_BYTES`] (its settings
/// fields and its `warning` take the room of three, its `mic` of three
/// more), a `cameras` line all
/// [`MAX_CAMERAS`]; image controls at their worst take more, and
/// [`snapshot_line`] cuts the controls, then the cameras, to keep the cap.
pub const MAX_PATH_BYTES: usize = 256;
/// Bound of the `path` of `preview` and `frame` lines (bytes of its JSON
/// encoding): those lines stay under [`MAX_LINE_BYTES`] with it. Such a path
/// is never clipped; a longer one is refused ([`preview_path_fits`]).
pub const MAX_PREVIEW_PATH_BYTES: usize = 3968;
/// At most this many image controls are listed; `controls_truncated` says
/// whether more were present (a daemon reports at most five).
pub const MAX_CONTROLS: usize = 16;
/// Bound of an image control's `name`.
pub const MAX_CONTROL_NAME_BYTES: usize = 32;
/// At most this many settings are in one `settings_group` line.
pub const MAX_SETTINGS_PER_LINE: usize = 8;
/// At most this many changes are in one `settings_delta` line.
pub const MAX_CHANGES_PER_LINE: usize = 16;
/// Bound of a setting id (`id`, and the `id` of a condition). A valid id is
/// at most this long already ([`obc_ipc::valid_setting_id`]).
pub const MAX_SETTING_ID_BYTES: usize = obc_ipc::settings::SETTING_ID_MAX;
/// Bound of a group id.
pub const MAX_GROUP_ID_BYTES: usize = 32;
/// Bound of a setting's or a group's `label`.
pub const MAX_LABEL_BYTES: usize = 48;
/// Bound of `disabled_reason`.
pub const MAX_REASON_BYTES: usize = 96;
/// Bound of an int setting's `unit`.
pub const MAX_UNIT_BYTES: usize = 16;
/// Bound of an option's `value`, and so of an enum setting's `value` and
/// `default` and of a string operand of a condition.
pub const MAX_OPTION_VALUE_BYTES: usize = 32;
/// Bound of an option's `label`.
pub const MAX_OPTION_LABEL_BYTES: usize = 32;
/// At most this many options of an enum setting are listed.
pub const MAX_OPTIONS: usize = 16;
/// At most this many `enabled_when` conditions of a setting are listed.
pub const MAX_CONDITIONS: usize = 4;
/// At most this many operands of an `in` condition (a list) are listed.
pub const MAX_CONDITION_VALUES: usize = 8;
/// Bound of a condition's `op` (`eq`, `ne`, `in`, or one a newer daemon
/// adds).
pub const MAX_OP_BYTES: usize = 8;
/// Bound of a setting's `hint`.
pub const MAX_HINT_BYTES: usize = 16;
/// Bound of a setting's `description` (revision 26).
pub const MAX_DESCRIPTION_BYTES: usize = 160;
/// Bound of a setting's `scope` (`camera` or `global`, or one a newer
/// daemon adds).
pub const MAX_SCOPE_BYTES: usize = 16;
/// Bound of a setting's `disabled_kind` (revision 28: `parent_off`, ...,
/// or one a newer daemon adds).
pub const MAX_DISABLED_KIND_BYTES: usize = 16;
/// At most this many `camera_health` warnings are listed (revision 20).
pub const MAX_HEALTH_WARNINGS: usize = 4;
/// Bound of a `camera_health` warning.
pub const MAX_HEALTH_WARNING_BYTES: usize = 320;
/// At most this many `camera_health` status lines are listed (revision
/// 26).
pub const MAX_HEALTH_STATUS: usize = 4;
/// Bound of a `camera_health` status line.
pub const MAX_HEALTH_STATUS_BYTES: usize = 96;
/// Bound of `camera_health`'s `shutter` and `show_mode` (`open`,
/// `desk_view`, or one a newer daemon adds).
pub const MAX_HEALTH_STATE_BYTES: usize = 16;
/// At most this many `camera_health` suggestions are listed.
pub const MAX_SUGGESTIONS: usize = 4;
/// Bound of a suggestion's `reason`.
pub const MAX_SUGGESTION_REASON_BYTES: usize = 160;
/// At most this many `camera_health` recommended changes are listed.
pub const MAX_RECOMMENDED: usize = 16;
/// Bound of a recommended change's `value` and `current` (as an option
/// label's).
pub const MAX_RECOMMENDED_VALUE_BYTES: usize = MAX_OPTION_LABEL_BYTES;
/// Appended to a clipped string.
pub const ELLIPSIS: &str = "\u{2026}";

/// `code` of an error line: the daemon is not running, or there is no
/// session bus.
pub const CODE_NOT_RUNNING: &str = "daemon-not-running";
/// `code` of an error line: any other D-Bus failure (an unexpected reply, a
/// timeout, a lost connection).
pub const CODE_DBUS: &str = "dbus-error";
/// `code` of an error line: the daemon is too old for the request (its
/// interface revision; `preview` needs 5).
pub const CODE_UNSUPPORTED: &str = "unsupported-daemon";
/// `code` of an error line: the daemon's preview frame path is longer than
/// [`MAX_PREVIEW_PATH_BYTES`], so no `frame` line could carry it.
pub const CODE_PREVIEW_PATH_TOO_LONG: &str = "preview-path-too-long";

/// Bytes `c` takes inside a JSON string as serde_json writes it.
fn escaped_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\u{08}' | '\u{0c}' | '\n' | '\r' | '\t' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// Bytes of the JSON encoding of `s`, without the quotes.
fn encoded_len(s: &str) -> usize {
    s.chars().map(escaped_len).sum()
}

/// Whether `path` can be the `path` of `preview` and `frame` lines: its
/// JSON encoding is at most [`MAX_PREVIEW_PATH_BYTES`].
pub fn preview_path_fits(path: &str) -> bool {
    encoded_len(path) <= MAX_PREVIEW_PATH_BYTES
}

/// `s`, clipped so that its JSON encoding (without the quotes) is at most
/// `max` bytes, ending in [`ELLIPSIS`] when it was clipped.
pub fn clip(s: &str, max: usize) -> Cow<'_, str> {
    if encoded_len(s) <= max {
        return Cow::Borrowed(s);
    }
    let budget = max.saturating_sub(ELLIPSIS.len());
    let mut used = 0;
    let mut end = 0;
    for (i, c) in s.char_indices() {
        let n = escaped_len(c);
        if used + n > budget {
            break;
        }
        used += n;
        end = i + c.len_utf8();
    }
    Cow::Owned(format!("{}{ELLIPSIS}", &s[..end]))
}

/// `s`, clipped so that its JSON encoding (without the quotes) is at most
/// `max` bytes, starting with [`ELLIPSIS`] when it was clipped: the end of
/// `s` is kept. For paths, whose file name is at the end.
pub fn clip_start(s: &str, max: usize) -> Cow<'_, str> {
    if encoded_len(s) <= max {
        return Cow::Borrowed(s);
    }
    let budget = max.saturating_sub(ELLIPSIS.len());
    let mut used = 0;
    let mut start = s.len();
    for (i, c) in s.char_indices().rev() {
        let n = escaped_len(c);
        if used + n > budget {
            break;
        }
        used += n;
        start = i;
    }
    Cow::Owned(format!("{ELLIPSIS}{}", &s[start..]))
}

/// `blur_strength` as printed: within 0 to 1 (a non-finite value is 0), so
/// it is always a JSON number.
fn strength(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// A stats value as printed: a finite, non-negative measurement, or `-1`.
fn measurement(value: f64) -> f64 {
    if value.is_finite() && Stats::is_measured(value) {
        value
    } else {
        Stats::NOT_MEASURABLE
    }
}

#[derive(Serialize)]
struct Camera<'a> {
    id: Cow<'a, str>,
    name: Cow<'a, str>,
    usb: Cow<'a, str>,
    active: bool,
}

/// The scalar fields shared by `snapshot` and `state` lines.
#[derive(Serialize)]
struct StateFields<'a> {
    state: &'static str,
    enabled: bool,
    error: Cow<'a, str>,
    /// `Warning` (revision 11); `""` from a daemon too old to have it.
    warning: Cow<'a, str>,
    active_camera: Cow<'a, str>,
    active_camera_name: Cow<'a, str>,
    consumers: u32,
    loopback_consumers: u32,
    pipewire_consumers: u32,
    preview_consumers: u32,
    pipewire_output: Cow<'a, str>,
    output_mode: Cow<'a, str>,
    effect: Cow<'a, str>,
    blur_strength: f64,
    background_image: Cow<'a, str>,
    /// `null` from a daemon too old to have it.
    denoise: Option<bool>,
    interface_revision: u32,
    /// `StageValues` (revision 16); `[]` when none, and from a daemon too
    /// old to have it.
    stage_values: Vec<StageValueField<'a>>,
}

/// One entry of `stage_values`: `{"stage":"auto-frame","name":"zoom","value":1.24}`.
#[derive(Serialize)]
struct StageValueField<'a> {
    stage: Cow<'a, str>,
    name: Cow<'a, str>,
    value: f64,
}

/// The `stage_values` of `s`: at most [`MAX_STAGE_VALUES`], a value that is
/// not a finite number left out (JSON has none).
fn stage_values_of(s: &Snapshot) -> Vec<StageValueField<'_>> {
    s.stage_values
        .iter()
        .filter(|v| v.value.is_finite())
        .take(MAX_STAGE_VALUES)
        .map(|v| StageValueField {
            stage: clip(&v.stage, MAX_STAGE_VALUE_WORD_BYTES),
            name: clip(&v.name, MAX_STAGE_VALUE_WORD_BYTES),
            value: v.value,
        })
        .collect()
}

impl<'a> StateFields<'a> {
    fn of(s: &'a Snapshot) -> Self {
        StateFields {
            state: s.state.as_str(),
            enabled: s.enabled,
            error: clip(&s.error, MAX_ERROR_BYTES),
            warning: clip(&s.warning, MAX_WARNING_BYTES),
            active_camera: clip(&s.active_camera, MAX_ID_BYTES),
            active_camera_name: clip(model::active_camera_name(s).unwrap_or(""), MAX_NAME_BYTES),
            consumers: s.consumers,
            loopback_consumers: s.loopback_consumers,
            pipewire_consumers: s.pipewire_consumers,
            preview_consumers: s.preview_consumers,
            pipewire_output: clip(&s.pipewire_output, MAX_PIPEWIRE_OUTPUT_BYTES),
            output_mode: clip(&s.output_mode, MAX_MODE_BYTES),
            effect: clip(&s.effect, MAX_EFFECT_BYTES),
            blur_strength: strength(s.blur_strength),
            background_image: clip_start(&s.background_image, MAX_PATH_BYTES),
            denoise: model::denoise(s),
            interface_revision: s.interface_revision,
            stage_values: stage_values_of(s),
        }
    }
}

/// A rate of `camera_health` as printed: a finite, non-negative value, or
/// `-1` (not measured).
fn rate(value: f64) -> f64 {
    if value.is_finite() && value >= 0.0 {
        value
    } else {
        obc_ipc::NOT_MEASURED
    }
}

/// One suggestion of `camera_health`.
#[derive(Serialize)]
struct SuggestionField<'a> {
    setting: Cow<'a, str>,
    value: Cow<'a, str>,
    reason: Cow<'a, str>,
}

/// One recommended change of `camera_health`.
#[derive(Serialize)]
struct RecommendedField<'a> {
    setting: Cow<'a, str>,
    label: Cow<'a, str>,
    value: Cow<'a, str>,
    current: Cow<'a, str>,
}

/// How many entries of each `camera_health` list are printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HealthLimits {
    warnings: usize,
    suggestions: usize,
    recommended: usize,
    status: usize,
}

impl HealthLimits {
    /// The bounds, or fewer when `h` has fewer.
    fn of(h: &CameraHealth) -> Self {
        HealthLimits {
            warnings: h.warnings.len().min(MAX_HEALTH_WARNINGS),
            suggestions: h.suggestions.len().min(MAX_SUGGESTIONS),
            recommended: h.recommended.len().min(MAX_RECOMMENDED),
            status: h.status.len().min(MAX_HEALTH_STATUS),
        }
    }

    /// One entry fewer: a recommended change, else a suggestion, else a
    /// warning, else a status line; `false` when every list is empty
    /// already.
    fn shrink(&mut self) -> bool {
        let list = if self.recommended > 0 {
            &mut self.recommended
        } else if self.suggestions > 0 {
            &mut self.suggestions
        } else if self.warnings > 0 {
            &mut self.warnings
        } else if self.status > 0 {
            &mut self.status
        } else {
            return false;
        };
        *list -= 1;
        true
    }
}

/// The camera's health (revision 20): the `camera_health` of a `snapshot`
/// line, and the body of a `camera_health` line.
#[derive(Serialize)]
struct HealthFields<'a> {
    camera: Cow<'a, str>,
    delivered_fps: f64,
    nominal_fps: f64,
    corrupt_frames: u64,
    warnings: Vec<Cow<'a, str>>,
    suggestions: Vec<SuggestionField<'a>>,
    has_recommendation: bool,
    recommended: Vec<RecommendedField<'a>>,
    shutter: Cow<'a, str>,
    show_mode: Cow<'a, str>,
    status: Vec<Cow<'a, str>>,
}

impl<'a> HealthFields<'a> {
    fn of(h: &'a CameraHealth, limits: HealthLimits) -> Self {
        HealthFields {
            camera: clip(&h.camera, MAX_ID_BYTES),
            delivered_fps: rate(h.delivered_fps),
            nominal_fps: rate(h.nominal_fps),
            corrupt_frames: h.corrupt_frames,
            warnings: h
                .warnings
                .iter()
                .take(limits.warnings)
                .map(|w| clip(w, MAX_HEALTH_WARNING_BYTES))
                .collect(),
            suggestions: h
                .suggestions
                .iter()
                .take(limits.suggestions)
                .map(|s| SuggestionField {
                    setting: clip(&s.setting, MAX_SETTING_ID_BYTES),
                    value: clip(&s.value, MAX_OPTION_VALUE_BYTES),
                    reason: clip(&s.reason, MAX_SUGGESTION_REASON_BYTES),
                })
                .collect(),
            has_recommendation: h.has_recommendation,
            recommended: h
                .recommended
                .iter()
                .take(limits.recommended)
                .map(|r| RecommendedField {
                    setting: clip(&r.setting, MAX_SETTING_ID_BYTES),
                    label: clip(&r.label, MAX_LABEL_BYTES),
                    value: clip(&r.value, MAX_RECOMMENDED_VALUE_BYTES),
                    current: clip(&r.current, MAX_RECOMMENDED_VALUE_BYTES),
                })
                .collect(),
            shutter: clip(&h.shutter, MAX_HEALTH_STATE_BYTES),
            show_mode: clip(&h.show_mode, MAX_HEALTH_STATE_BYTES),
            status: h
                .status
                .iter()
                .take(limits.status)
                .map(|l| clip(l, MAX_HEALTH_STATUS_BYTES))
                .collect(),
        }
    }
}

#[derive(Serialize)]
struct HealthLine<'a> {
    v: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(flatten)]
    fields: HealthFields<'a>,
}

/// `{"v":1,"type":"camera_health",...}`: the camera's health (revision
/// 20), printed by `recommended --json`, `recommended apply --json` and
/// by `watch --json` when it changes. Its lists are cut should the line
/// exceed [`MAX_LINE_BYTES`].
pub fn camera_health_line(h: &CameraHealth) -> String {
    let mut limits = HealthLimits::of(h);
    loop {
        let line = to_line(&HealthLine {
            v: VERSION,
            kind: "camera_health",
            fields: HealthFields::of(h, limits),
        });
        if line.len() <= MAX_LINE_BYTES || !limits.shrink() {
            return line;
        }
    }
}

fn cameras_of<'a>(s: &'a Snapshot, limit: usize) -> (Vec<Camera<'a>>, bool) {
    let list = s
        .cameras
        .iter()
        .take(limit)
        .map(|c: &CameraInfo| Camera {
            id: clip(&c.id, MAX_ID_BYTES),
            name: clip(&c.name, MAX_NAME_BYTES),
            usb: clip(&c.usb, MAX_USB_BYTES),
            active: !c.id.is_empty() && c.id == s.active_camera,
        })
        .collect();
    (list, s.cameras.len() > limit)
}

/// One image control.
#[derive(Serialize)]
struct Control<'a> {
    name: Cow<'a, str>,
    min: i32,
    max: i32,
    step: i32,
    default: i32,
    value: i32,
    stored: bool,
}

/// The image controls of one camera: the `camera_controls` of a `snapshot`
/// line, and the body of a `controls` line.
#[derive(Serialize)]
struct Controls<'a> {
    camera: Cow<'a, str>,
    controls: Vec<Control<'a>>,
    controls_truncated: bool,
}

impl<'a> Controls<'a> {
    /// `c`, with at most `limit` controls.
    fn of(c: &'a CameraControls, limit: usize) -> Self {
        Controls {
            camera: clip(&c.camera, MAX_ID_BYTES),
            controls: c
                .controls
                .iter()
                .take(limit)
                .map(|c: &CameraControl| Control {
                    name: clip(&c.name, MAX_CONTROL_NAME_BYTES),
                    min: c.min,
                    max: c.max,
                    step: c.step,
                    default: c.default,
                    value: c.value,
                    stored: c.stored,
                })
                .collect(),
            controls_truncated: c.controls.len() > limit,
        }
    }
}

/// The microphone's fields (revision 13): the `mic` of a `snapshot` line,
/// and the body of a `mic` line.
#[derive(Serialize)]
struct MicFields<'a> {
    state: Cow<'a, str>,
    consumers: u32,
    provider: Cow<'a, str>,
    latency_ms: f64,
    error: Cow<'a, str>,
}

impl<'a> MicFields<'a> {
    fn of(m: &'a MicStatus) -> Self {
        MicFields {
            state: clip(&m.state, MAX_MIC_STATE_BYTES),
            consumers: m.consumers,
            provider: clip(&m.provider, MAX_MIC_PROVIDER_BYTES),
            latency_ms: if m.latency_ms.is_finite() && m.latency_ms >= 0.0 {
                m.latency_ms
            } else {
                0.0
            },
            error: clip(&m.error, MAX_MIC_ERROR_BYTES),
        }
    }
}

#[derive(Serialize)]
struct MicLine<'a> {
    v: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(flatten)]
    fields: MicFields<'a>,
}

/// `{"v":1,"type":"mic",...}`: the microphone (revision 13), printed by
/// `mic status --json` and by `watch --json` when it changes.
pub fn mic_line(m: &MicStatus) -> String {
    to_line(&MicLine {
        v: VERSION,
        kind: "mic",
        fields: MicFields::of(m),
    })
}

#[derive(Serialize)]
struct SnapshotLine<'a> {
    v: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(flatten)]
    fields: StateFields<'a>,
    cameras: Vec<Camera<'a>>,
    cameras_truncated: bool,
    /// `null` from a daemon too old to have it.
    camera_controls: Option<Controls<'a>>,
    /// `SettingsRevision`; `null` before revision 7.
    settings_revision: Option<u32>,
    /// The settings schema version the daemon's settings follow
    /// ([`SETTINGS_SCHEMA`]); `null` before revision 7.
    settings_schema: Option<u32>,
    /// `ModulesSerial`; `0` before revision 9.
    modules_serial: u32,
    /// `ConfigSerial`; `0` before revision 10.
    config_serial: u32,
    /// The microphone; `null` before revision 13.
    mic: Option<MicFields<'a>>,
    /// The camera's health; `null` before revision 20.
    camera_health: Option<HealthFields<'a>>,
}

#[derive(Serialize)]
struct ControlsLine<'a> {
    v: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(flatten)]
    controls: Controls<'a>,
}

#[derive(Serialize)]
struct StateLine<'a> {
    v: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    changed: &'a [&'a str],
    #[serde(flatten)]
    fields: StateFields<'a>,
}

#[derive(Serialize)]
struct CamerasLine<'a> {
    v: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    active_camera: Cow<'a, str>,
    cameras: Vec<Camera<'a>>,
    cameras_truncated: bool,
}

#[derive(Serialize)]
struct StatsLine {
    v: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    fps: f64,
    latency_p50_ms: f64,
    cpu_percent: f64,
    gpu_percent: f64,
}

#[derive(Serialize)]
struct PreviewLine<'a> {
    v: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    fps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<&'a str>,
}

#[derive(Serialize)]
struct FrameLine<'a> {
    v: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    seq: u32,
    path: &'a str,
}

/// `config_changed` and `modules_changed`.
#[derive(Serialize)]
struct SerialLine {
    v: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    serial: u32,
}

#[derive(Serialize)]
struct ErrorLine<'a> {
    v: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    code: &'a str,
    error: Cow<'a, str>,
}

fn to_line<T: Serialize>(value: &T) -> String {
    // Serialising these plain structs cannot fail: every map key is a string.
    serde_json::to_string(value).expect("JSON line serialisation")
}

/// Serialises a camera-bearing line with the full bound of cameras, and with
/// fewer if the line would exceed [`MAX_LINE_BYTES`] (which the field bounds
/// already prevent; this keeps the cap a guarantee).
fn bounded_cameras_line<'a>(
    s: &'a Snapshot,
    build: impl Fn(Vec<Camera<'a>>, bool) -> String,
) -> String {
    let mut limit = MAX_CAMERAS;
    loop {
        let (cameras, truncated) = cameras_of(s, limit);
        let line = build(cameras, truncated);
        if line.len() <= MAX_LINE_BYTES || limit == 0 {
            return line;
        }
        limit -= 1;
    }
}

/// `{"v":1,"type":"config_changed","serial":N}`: the daemon's
/// `ConfigChanged` signal (revision 10).
pub fn config_changed_line(serial: u32) -> String {
    to_line(&SerialLine {
        v: VERSION,
        kind: "config_changed",
        serial,
    })
}

/// `{"v":1,"type":"modules_changed","serial":N}`: `ModulesSerial` moved
/// (revision 9).
pub fn modules_changed_line(serial: u32) -> String {
    to_line(&SerialLine {
        v: VERSION,
        kind: "modules_changed",
        serial,
    })
}

/// `{"v":1,"type":"snapshot",...}`: every property. Should the line exceed
/// [`MAX_LINE_BYTES`] (which only strings and lists at their bounds can
/// make it do), its image controls are cut first, then the lists of its
/// `camera_health`, then its cameras.
pub fn snapshot_line(s: &Snapshot) -> String {
    let build = |camera_limit: usize, control_limit: usize, health: Option<HealthLimits>| {
        let (cameras, cameras_truncated) = cameras_of(s, camera_limit);
        to_line(&SnapshotLine {
            v: VERSION,
            kind: "snapshot",
            fields: StateFields::of(s),
            cameras,
            cameras_truncated,
            camera_controls: s
                .camera_controls
                .as_ref()
                .map(|c| Controls::of(c, control_limit)),
            settings_revision: model::settings_revision(s),
            settings_schema: model::has_settings(s).then_some(SETTINGS_SCHEMA),
            modules_serial: s.modules_serial.unwrap_or(0),
            config_serial: s.config_serial.unwrap_or(0),
            mic: s.mic.as_ref().map(MicFields::of),
            camera_health: model::camera_health(s)
                .zip(health)
                .map(|(h, l)| HealthFields::of(h, l)),
        })
    };
    let mut cameras = MAX_CAMERAS;
    let mut controls = s
        .camera_controls
        .as_ref()
        .map_or(0, |c| c.controls.len().min(MAX_CONTROLS));
    let mut health = model::camera_health(s).map(HealthLimits::of);
    loop {
        let line = build(cameras, controls, health);
        if line.len() <= MAX_LINE_BYTES {
            return line;
        }
        if controls > 0 {
            controls -= 1;
        } else if health.as_mut().is_some_and(HealthLimits::shrink) {
        } else if cameras > 0 {
            cameras -= 1;
        } else {
            return line;
        }
    }
}

/// `{"v":1,"type":"state",...}`: every scalar field, after a change of the
/// fields named in `changed`.
pub fn state_line(s: &Snapshot, changed: &[&str]) -> String {
    to_line(&StateLine {
        v: VERSION,
        kind: "state",
        changed,
        fields: StateFields::of(s),
    })
}

/// `{"v":1,"type":"cameras",...}`: the camera list and the active camera.
pub fn cameras_line(s: &Snapshot) -> String {
    bounded_cameras_line(s, |cameras, cameras_truncated| {
        to_line(&CamerasLine {
            v: VERSION,
            kind: "cameras",
            active_camera: clip(&s.active_camera, MAX_ID_BYTES),
            cameras,
            cameras_truncated,
        })
    })
}

/// `{"v":1,"type":"controls","camera":...,"controls":[...],
/// "controls_truncated":false}`: the image controls of one camera.
pub fn controls_line(c: &CameraControls) -> String {
    to_line(&ControlsLine {
        v: VERSION,
        kind: "controls",
        controls: Controls::of(c, MAX_CONTROLS),
    })
}

/// `{"v":1,"type":"stats",...}`: one `Stats` signal; `-1` is not measurable.
pub fn stats_line(st: &Stats) -> String {
    to_line(&StatsLine {
        v: VERSION,
        kind: "stats",
        fps: measurement(st.fps),
        latency_p50_ms: measurement(st.latency_p50_ms),
        cpu_percent: measurement(st.cpu_percent),
        gpu_percent: measurement(st.gpu_percent),
    })
}

/// `{"v":1,"type":"preview","active":true,"fps":15,"path":...}` once the
/// preview lease is held; `{"v":1,"type":"preview","active":false}` when it
/// is lost. The path is never clipped: the caller checks
/// [`preview_path_fits`] first.
pub fn preview_line(held: Option<(u32, &str)>) -> String {
    to_line(&PreviewLine {
        v: VERSION,
        kind: "preview",
        active: held.is_some(),
        fps: held.map(|(fps, _)| fps),
        path: held.map(|(_, path)| path),
    })
}

/// `{"v":1,"type":"frame","seq":N,"path":...}`: the frame file was
/// replaced. The path is never clipped: the caller checks
/// [`preview_path_fits`] first.
pub fn frame_line(seq: u32, path: &str) -> String {
    to_line(&FrameLine {
        v: VERSION,
        kind: "frame",
        seq,
        path,
    })
}

/// `{"v":1,"type":"error","code":...,"error":...}`.
pub fn error_line(code: &str, message: &str) -> String {
    to_line(&ErrorLine {
        v: VERSION,
        kind: "error",
        code,
        error: clip(message, MAX_ERROR_BYTES),
    })
}

/// A value of the settings channel as printed: a boolean, a number, a
/// string, or a list of those (the operands of an `in` condition).
#[derive(Serialize)]
#[serde(untagged)]
enum JsonValue<'a> {
    Bool(bool),
    Int(i64),
    Str(Cow<'a, str>),
    List(Vec<JsonValue<'a>>),
}

impl<'a> JsonValue<'a> {
    /// `v` as printed: a string clipped to [`MAX_OPTION_VALUE_BYTES`], a
    /// list cut at [`MAX_CONDITION_VALUES`] items, a list within a list
    /// printed empty.
    fn of(v: &'a SettingValue) -> Self {
        Self::at_depth(v, 0)
    }

    fn at_depth(v: &'a SettingValue, depth: usize) -> Self {
        match v {
            SettingValue::Bool(b) => JsonValue::Bool(*b),
            SettingValue::Int(i) => JsonValue::Int(*i),
            SettingValue::Str(s) => JsonValue::Str(clip(s, MAX_OPTION_VALUE_BYTES)),
            SettingValue::List(_) if depth > 0 => JsonValue::List(Vec::new()),
            SettingValue::List(items) => JsonValue::List(
                items
                    .iter()
                    .take(MAX_CONDITION_VALUES)
                    .map(|i| Self::at_depth(i, depth + 1))
                    .collect(),
            ),
        }
    }
}

/// One `enabled_when` condition.
#[derive(Serialize)]
struct ConditionJson<'a> {
    id: Cow<'a, str>,
    op: Cow<'a, str>,
    value: JsonValue<'a>,
}

/// One option of an enum setting.
#[derive(Serialize)]
struct OptionJson<'a> {
    value: Cow<'a, str>,
    label: Cow<'a, str>,
}

/// One setting of a `settings_group` line, with the keys of its type only.
#[derive(Serialize)]
struct SettingJson<'a> {
    id: Cow<'a, str>,
    label: Cow<'a, str>,
    /// Where it is kept: `camera` (per camera) or `global` (the same for
    /// every camera, revision 8), which a client routes by.
    scope: Cow<'a, str>,
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<JsonValue<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default: Option<JsonValue<'a>>,
    stored: bool,
    enabled: bool,
    disabled_reason: Cow<'a, str>,
    /// Revision 28: always present, `""` while enabled or from an older
    /// daemon.
    disabled_kind: Cow<'a, str>,
    enabled_when: Vec<ConditionJson<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    min: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    step: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unit: Option<Cow<'a, str>>,
    /// Revision 27: an int is shown as `(value - display_offset) *
    /// display_scale`, signed; both absent when it is shown as it is, as
    /// on the bus.
    #[serde(skip_serializing_if = "Option::is_none")]
    display_offset: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_scale: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<Vec<OptionJson<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<Cow<'a, str>>,
    /// Revision 26; absent when the setting has none, as on the bus.
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<Cow<'a, str>>,
}

impl<'a> SettingJson<'a> {
    /// `s` as printed, or `None` for a type this client does not know.
    fn of(s: &'a Setting) -> Option<Self> {
        let mut j = SettingJson {
            id: clip(&s.id, MAX_SETTING_ID_BYTES),
            label: clip(&s.label, MAX_LABEL_BYTES),
            scope: clip(&s.scope, MAX_SCOPE_BYTES),
            kind: "",
            value: None,
            default: None,
            stored: s.stored,
            enabled: s.enabled,
            disabled_reason: clip(&s.disabled_reason, MAX_REASON_BYTES),
            disabled_kind: clip(&s.disabled_kind, MAX_DISABLED_KIND_BYTES),
            enabled_when: s
                .enabled_when
                .iter()
                .take(MAX_CONDITIONS)
                .map(|c: &Condition| ConditionJson {
                    id: clip(&c.id, MAX_SETTING_ID_BYTES),
                    op: clip(c.op.as_str(), MAX_OP_BYTES),
                    value: JsonValue::of(&c.value),
                })
                .collect(),
            min: None,
            max: None,
            step: None,
            unit: None,
            display_offset: None,
            display_scale: None,
            options: None,
            hint: s.hint.as_deref().map(|h| clip(h, MAX_HINT_BYTES)),
            description: s
                .description
                .as_deref()
                .map(|d| clip(d, MAX_DESCRIPTION_BYTES)),
        };
        match &s.kind {
            SettingKind::Bool { default, value } => {
                j.kind = "bool";
                j.value = Some(JsonValue::Bool(*value));
                j.default = Some(JsonValue::Bool(*default));
            }
            SettingKind::Int {
                min,
                max,
                step,
                default,
                value,
                unit,
                display,
            } => {
                j.kind = "int";
                j.value = Some(JsonValue::Int(*value));
                j.default = Some(JsonValue::Int(*default));
                j.min = Some(*min);
                j.max = Some(*max);
                j.step = Some(*step);
                j.unit = unit.as_deref().map(|u| clip(u, MAX_UNIT_BYTES));
                j.display_offset = display.map(|d| d.offset);
                j.display_scale = display.map(|d| d.scale);
            }
            SettingKind::Enum {
                options,
                default,
                value,
            } => {
                j.kind = "enum";
                j.value = Some(JsonValue::Str(clip(value, MAX_OPTION_VALUE_BYTES)));
                j.default = Some(JsonValue::Str(clip(default, MAX_OPTION_VALUE_BYTES)));
                j.options = Some(
                    options
                        .iter()
                        .take(MAX_OPTIONS)
                        .map(|(v, l)| OptionJson {
                            value: clip(v, MAX_OPTION_VALUE_BYTES),
                            label: clip(l, MAX_OPTION_LABEL_BYTES),
                        })
                        .collect(),
                );
            }
            SettingKind::Action => j.kind = "action",
            SettingKind::Unknown(_) => return None,
        }
        Some(j)
    }
}

/// The `group` of a `settings_group` line.
#[derive(Serialize)]
struct GroupJson<'a, 'b> {
    id: Cow<'a, str>,
    label: Cow<'a, str>,
    /// Where a client shows the group (revision 13: `mic`); left out when
    /// the daemon gives none.
    #[serde(skip_serializing_if = "Option::is_none")]
    surface: Option<Cow<'a, str>>,
    settings: &'b [SettingJson<'a>],
}

#[derive(Serialize)]
struct SettingsGroupLine<'a, 'b> {
    v: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    schema: u32,
    rev: u32,
    serial: u32,
    camera: Cow<'a, str>,
    index: usize,
    count: usize,
    part: usize,
    parts: usize,
    group: GroupJson<'a, 'b>,
}

/// One change of a `settings_delta` line.
#[derive(Serialize)]
struct ChangeJson<'a> {
    id: Cow<'a, str>,
    /// Absent for an action.
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<JsonValue<'a>>,
    stored: bool,
    enabled: bool,
    disabled_reason: Cow<'a, str>,
    /// Revision 28, as in a setting.
    disabled_kind: Cow<'a, str>,
}

#[derive(Serialize)]
struct SettingsDeltaLine<'a, 'b> {
    v: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    schema: u32,
    rev: u32,
    serial: u32,
    camera: Cow<'a, str>,
    changes: &'b [ChangeJson<'a>],
}

#[derive(Serialize)]
struct SettingsClearLine {
    v: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    rev: u32,
    serial: u32,
}

/// Splits `items` (each with the length of its JSON encoding) into runs of
/// at most `max_items` whose JSON array, in a line of `overhead` bytes
/// with an empty array, keeps the line within [`MAX_LINE_BYTES`]: filled
/// greedily, in order. An item too long to fit a line on its own (which the
/// bounds prevent) is left out, so that the cap stays a guarantee.
pub(crate) fn pack<T>(items: Vec<(T, usize)>, overhead: usize, max_items: usize) -> Vec<Vec<T>> {
    let mut runs = Vec::new();
    let mut run: Vec<T> = Vec::new();
    // Bytes of the run's items and the commas between them.
    let mut used = 0;
    for (item, len) in items {
        if overhead + len > MAX_LINE_BYTES {
            continue;
        }
        let comma = usize::from(!run.is_empty());
        if run.len() == max_items || overhead + used + comma + len > MAX_LINE_BYTES {
            runs.push(std::mem::take(&mut run));
            used = 0;
        }
        used += usize::from(!run.is_empty()) + len;
        run.push(item);
    }
    if !run.is_empty() {
        runs.push(run);
    }
    runs
}

/// The settings of `g` as printed, with their encoded lengths; settings of
/// an unknown type left out.
fn encoded_settings(g: &SettingsGroup) -> Vec<(SettingJson<'_>, usize)> {
    g.settings
        .iter()
        .filter_map(SettingJson::of)
        .map(|j| {
            let len = to_line(&j).len();
            (j, len)
        })
        .collect()
}

/// The full set of settings of `tree`: one `settings_group` line per group,
/// or per part of a group that does not fit one line (`part` 1 to `parts`;
/// `1` of `1` when whole), each with at most [`MAX_SETTINGS_PER_LINE`]
/// settings, `index` 0 to `count - 1` over the whole set:
///
/// `{"v":1,"type":"settings_group","schema":1,"rev":R,"serial":S,"camera":"<id>",
/// "index":I,"count":N,"part":P,"parts":Q,"group":{"id":...,"label":...,"settings":[...]}}`
///
/// The camera's groups come first, then the global ones (revision 8: the
/// video effects), which are there with no camera too (`camera` is then
/// `""`). A tree with nothing to show (no camera and no global setting, or
/// no setting this CLI can show) is one [`settings_clear_line`] instead.
pub fn settings_lines(tree: &SettingsTree) -> Vec<String> {
    let camera = clip(&tree.camera, MAX_ID_BYTES);
    let line = |group: &SettingsGroup,
                settings: &[SettingJson<'_>],
                (index, count, part, parts): (usize, usize, usize, usize)| {
        to_line(&SettingsGroupLine {
            v: VERSION,
            kind: "settings_group",
            schema: SETTINGS_SCHEMA,
            rev: tree.revision,
            serial: tree.serial,
            camera: camera.clone(),
            index,
            count,
            part,
            parts,
            group: GroupJson {
                id: clip(&group.id, MAX_GROUP_ID_BYTES),
                label: clip(&group.label, MAX_LABEL_BYTES),
                surface: group.surface.as_deref().map(|s| clip(s, MAX_SCOPE_BYTES)),
                settings,
            },
        })
    };
    // The lines' numbers are not known before the packing: measure with the
    // longest they can be.
    let widest = u32::MAX as usize;
    let mut runs: Vec<(&SettingsGroup, Vec<Vec<SettingJson<'_>>>)> = Vec::new();
    // A camera's groups need the camera; the global ones (revision 8) are
    // there without one.
    for g in &tree.groups {
        if tree.camera.is_empty() && !g.is_global() {
            continue;
        }
        let overhead = line(g, &[], (widest, widest, widest, widest)).len();
        let parts = pack(encoded_settings(g), overhead, MAX_SETTINGS_PER_LINE);
        if !parts.is_empty() {
            runs.push((g, parts));
        }
    }
    let count: usize = runs.iter().map(|(_, parts)| parts.len()).sum();
    if count == 0 {
        return vec![settings_clear_line(tree.revision, tree.serial)];
    }
    let mut lines = Vec::with_capacity(count);
    for (g, parts) in &runs {
        for (i, settings) in parts.iter().enumerate() {
            lines.push(line(g, settings, (lines.len(), count, i + 1, parts.len())));
        }
    }
    lines
}

/// The `settings_delta` lines of one `SettingsChanged` signal on `camera`:
///
/// `{"v":1,"type":"settings_delta","schema":1,"rev":R,"serial":S,"camera":"<id>",
/// "changes":[{"id":...,"value":...,"stored":...,"enabled":...,"disabled_reason":...}]}`
///
/// with at most [`MAX_CHANGES_PER_LINE`] changes per line (more make
/// several lines with the same `rev`), no `value` for an action. No change,
/// no line.
pub fn settings_delta_lines(
    rev: u32,
    serial: u32,
    camera: &str,
    changes: &[SettingChange],
) -> Vec<String> {
    let camera = clip(camera, MAX_ID_BYTES);
    let line = |changes: &[ChangeJson<'_>]| {
        to_line(&SettingsDeltaLine {
            v: VERSION,
            kind: "settings_delta",
            schema: SETTINGS_SCHEMA,
            rev,
            serial,
            camera: camera.clone(),
            changes,
        })
    };
    let encoded: Vec<(ChangeJson<'_>, usize)> = changes
        .iter()
        .map(|c| {
            let j = ChangeJson {
                id: clip(&c.id, MAX_SETTING_ID_BYTES),
                value: c.value.as_ref().map(JsonValue::of),
                stored: c.stored,
                enabled: c.enabled,
                disabled_reason: clip(&c.disabled_reason, MAX_REASON_BYTES),
                disabled_kind: clip(&c.disabled_kind, MAX_DISABLED_KIND_BYTES),
            };
            let len = to_line(&j).len();
            (j, len)
        })
        .collect();
    let overhead = line(&[]).len();
    pack(encoded, overhead, MAX_CHANGES_PER_LINE)
        .iter()
        .map(|run| line(run))
        .collect()
}

/// `{"v":1,"type":"settings_clear","rev":R,"serial":S}`: there is nothing
/// to show (no camera and no global setting, or no setting this CLI can
/// show).
pub fn settings_clear_line(rev: u32, serial: u32) -> String {
    to_line(&SettingsClearLine {
        v: VERSION,
        kind: "settings_clear",
        rev,
        serial,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use obc_ipc::State;
    use serde_json::Value;

    fn cam(id: &str, name: &str, usb: &str) -> CameraInfo {
        CameraInfo {
            id: id.into(),
            name: name.into(),
            usb: usb.into(),
        }
    }

    fn sample() -> Snapshot {
        Snapshot {
            state: State::Active,
            error: String::new(),
            warning: String::new(),
            enabled: true,
            cameras: vec![
                cam("usb-1234_abcd", "First Camera", "1234:abcd"),
                cam("platform-video2", "Second Camera", ""),
            ],
            active_camera: "platform-video2".into(),
            output_mode: "NV12 1920x1080@30".into(),
            effect: "blur".into(),
            blur_strength: 0.5,
            background_image: "/home/u/bg.png".into(),
            consumers: 2,
            loopback_consumers: 1,
            pipewire_consumers: 1,
            preview_consumers: 0,
            pipewire_output: "streaming".into(),
            denoise: true,
            camera_controls: None,
            settings_revision: None,
            settings_schema_serial: None,
            modules_serial: None,
            backend_profile: None,
            config_serial: None,
            mic: None,
            stage_values: Vec::new(),
            camera_health: None,
            interface_revision: 4,
        }
    }

    fn parse(line: &str) -> Value {
        assert!(!line.contains('\n'), "line has a newline: {line}");
        assert!(line.len() <= MAX_LINE_BYTES, "{} bytes", line.len());
        let v: Value = serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}"));
        assert_eq!(v["v"], 1, "{line}");
        assert!(v["type"].is_string(), "{line}");
        v
    }

    #[test]
    fn the_warning_is_a_string_of_the_snapshot_and_state_lines() {
        let mut s = sample();
        s.warning = "effect blur unavailable: NVIDIA Maxine video effects: Disabled".into();
        let v = parse(&snapshot_line(&s));
        assert_eq!(v["warning"], s.warning.as_str());
        assert_eq!(v["state"], "active", "not an error");
        let old = sample();
        let changes = model::diff(&old, &s);
        assert_eq!(changes.fields, ["warning"]);
        let v = parse(&state_line(&s, &changes.fields));
        assert_eq!(v["warning"], s.warning.as_str());
        assert_eq!(v["changed"], serde_json::json!(["warning"]));
    }

    #[test]
    fn stage_values_are_in_the_snapshot_and_state_lines_bounded() {
        let mut s = sample();
        assert_eq!(
            parse(&snapshot_line(&s))["stage_values"],
            serde_json::json!([])
        );
        let value = |stage: &str, value: f64| obc_ipc::StageValue {
            stage: stage.into(),
            name: "zoom".into(),
            value,
        };
        let old = s.clone();
        s.stage_values = vec![value("auto-frame", 1.24), value("x", f64::NAN)];
        s.stage_values
            .extend((0..20).map(|i| value(&"s".repeat(100 + i), 2.0)));
        let v = parse(&snapshot_line(&s));
        let list = v["stage_values"].as_array().unwrap();
        assert_eq!(list.len(), MAX_STAGE_VALUES, "{v}");
        assert_eq!(
            list[0],
            serde_json::json!({"stage": "auto-frame", "name": "zoom", "value": 1.24})
        );
        // The NaN is left out; long words are cut.
        assert!(list.iter().all(|e| e["stage"] != "x"), "{v}");
        assert!(list.iter().all(|e| e["value"].is_f64()), "{v}");
        assert!(list[1]["stage"].as_str().unwrap().starts_with("sss"), "{v}");
        assert!(list[1]["stage"].as_str().unwrap().len() <= MAX_STAGE_VALUE_WORD_BYTES);
        let changes = model::diff(&old, &s);
        assert_eq!(changes.fields, ["stage_values"]);
        let v = parse(&state_line(&s, &changes.fields));
        assert_eq!(v["stage_values"][0]["value"], 1.24);
        // A NaN is not a change every time.
        assert!(model::diff(&s, &s.clone()).is_empty());
    }

    #[test]
    fn snapshot_line_has_every_field() {
        let v = parse(&snapshot_line(&sample()));
        assert_eq!(v["type"], "snapshot");
        assert_eq!(v["state"], "active");
        assert_eq!(v["enabled"], true);
        assert_eq!(v["error"], "");
        assert_eq!(v["warning"], "", "none, and a daemon before revision 11");
        assert_eq!(v["active_camera"], "platform-video2");
        assert_eq!(v["active_camera_name"], "Second Camera");
        assert_eq!(v["consumers"], 2);
        assert_eq!(v["loopback_consumers"], 1);
        assert_eq!(v["pipewire_consumers"], 1);
        assert_eq!(v["preview_consumers"], 0);
        assert_eq!(v["pipewire_output"], "streaming");
        assert_eq!(v["output_mode"], "NV12 1920x1080@30");
        assert_eq!(v["effect"], "blur");
        assert_eq!(v["blur_strength"], 0.5);
        assert_eq!(v["background_image"], "/home/u/bg.png");
        assert_eq!(v["denoise"], true);
        assert_eq!(v["interface_revision"], 4);
        assert_eq!(v["cameras_truncated"], false);
        let cams = v["cameras"].as_array().unwrap();
        assert_eq!(cams.len(), 2);
        assert_eq!(cams[0]["id"], "usb-1234_abcd");
        assert_eq!(cams[0]["name"], "First Camera");
        assert_eq!(cams[0]["usb"], "1234:abcd");
        assert_eq!(cams[0]["active"], false);
        assert_eq!(cams[1]["usb"], "");
        assert_eq!(cams[1]["active"], true);
        // "v" and "type" lead the object.
        assert!(snapshot_line(&sample()).starts_with(r#"{"v":1,"type":"snapshot","#));
    }

    #[test]
    fn state_line_names_the_changes_and_carries_the_scalars() {
        let line = state_line(&sample(), &["state", "consumers"]);
        let v = parse(&line);
        assert_eq!(v["type"], "state");
        assert_eq!(v["changed"], serde_json::json!(["state", "consumers"]));
        assert_eq!(v["state"], "active");
        assert_eq!(v["active_camera_name"], "Second Camera");
        assert_eq!(v["blur_strength"], 0.5);
        assert_eq!(v["background_image"], "/home/u/bg.png");
        assert_eq!(v["loopback_consumers"], 1);
        assert_eq!(v["pipewire_consumers"], 1);
        assert_eq!(v["pipewire_output"], "streaming");
        assert!(v.get("cameras").is_none(), "{line}");
        // Every scalar of the model is in the line.
        for field in model::STATE_FIELDS {
            assert!(v.get(field).is_some(), "{field} missing from {line}");
        }
        // A daemon without the PipeWire output: its fields are there,
        // with the values of one that has none.
        let mut old = Snapshot::empty();
        old.consumers = 2;
        old.loopback_consumers = 2;
        let v = parse(&snapshot_line(&old));
        assert_eq!(
            (
                &v["consumers"],
                &v["loopback_consumers"],
                &v["pipewire_consumers"],
                &v["pipewire_output"]
            ),
            (
                &serde_json::json!(2),
                &serde_json::json!(2),
                &serde_json::json!(0),
                &serde_json::json!("off")
            )
        );
    }

    #[test]
    fn denoise_is_a_boolean_or_null_from_an_older_daemon() {
        let mut s = sample();
        for (denoise, revision, printed) in [
            (true, 4, serde_json::json!(true)),
            (false, 4, serde_json::json!(false)),
            (false, 3, Value::Null),
            (true, 3, Value::Null),
            (false, 0, Value::Null),
        ] {
            s.denoise = denoise;
            s.interface_revision = revision;
            for line in [snapshot_line(&s), state_line(&s, &["denoise"])] {
                let v = parse(&line);
                assert_eq!(v.get("denoise"), Some(&printed), "{line}");
            }
        }
    }

    #[test]
    fn blur_strength_is_always_a_number_from_0_to_1() {
        let mut s = sample();
        for (given, printed) in [
            (0.0, 0.0),
            (1.0, 1.0),
            (0.25, 0.25),
            (1.5, 1.0),
            (-0.5, 0.0),
            (f64::NAN, 0.0),
            (f64::INFINITY, 0.0),
        ] {
            s.blur_strength = given;
            for line in [snapshot_line(&s), state_line(&s, &["blur_strength"])] {
                let v = parse(&line);
                assert!(v["blur_strength"].is_number(), "{given}: {line}");
                assert_eq!(v["blur_strength"].as_f64(), Some(printed), "{given}");
            }
        }
    }

    #[test]
    fn background_image_is_clipped_and_stays_valid() {
        let mut s = sample();
        // Control characters (6-byte escapes), bidi overrides and other
        // multi-byte characters, newlines and quotes.
        let nasty = format!("/home/u/{}", "\u{1}\u{202e}\u{2066}é🎥\n\"\\".repeat(200));
        s.background_image = nasty.clone();
        for line in [snapshot_line(&s), state_line(&s, &model::STATE_FIELDS)] {
            let v = parse(&line);
            let img = v["background_image"].as_str().unwrap();
            assert!(img.starts_with(ELLIPSIS), "{img}");
            assert!(nasty.ends_with(img.trim_start_matches(ELLIPSIS)));
            let encoded = serde_json::to_string(img).unwrap();
            assert!(encoded.len() - 2 <= MAX_PATH_BYTES, "{}", encoded.len());
        }
        // A long path keeps its file name: the start is what is clipped.
        s.background_image = format!("/home/u/{}office.jpg", "deep/".repeat(100));
        let v = parse(&snapshot_line(&s));
        let img = v["background_image"].as_str().unwrap();
        assert!(img.starts_with(ELLIPSIS), "{img}");
        assert!(img.ends_with("/deep/office.jpg"), "{img}");
        assert_eq!(img.len(), MAX_PATH_BYTES, "{img}");
        // A short path is printed as it is.
        s.background_image = "/home/u/\u{202e}gnp.exe".into();
        let v = parse(&snapshot_line(&s));
        assert_eq!(v["background_image"], "/home/u/\u{202e}gnp.exe");
    }

    #[test]
    fn cameras_line_lists_cameras() {
        let v = parse(&cameras_line(&sample()));
        assert_eq!(v["type"], "cameras");
        assert_eq!(v["active_camera"], "platform-video2");
        assert_eq!(v["cameras"].as_array().unwrap().len(), 2);
        assert_eq!(v["cameras_truncated"], false);

        let mut none = sample();
        none.cameras.clear();
        none.active_camera.clear();
        let v = parse(&cameras_line(&none));
        assert_eq!(v["cameras"], serde_json::json!([]));
        assert_eq!(v["active_camera"], "");
        let v = parse(&snapshot_line(&none));
        assert_eq!(v["active_camera_name"], "");
    }

    #[test]
    fn stats_line_uses_minus_one_for_unknown() {
        let v = parse(&stats_line(&Stats {
            fps: 30.0,
            latency_p50_ms: 12.5,
            cpu_percent: f64::NAN,
            gpu_percent: -1.0,
        }));
        assert_eq!(v["type"], "stats");
        assert_eq!(v["fps"], 30.0);
        assert_eq!(v["latency_p50_ms"], 12.5);
        assert_eq!(v["cpu_percent"], -1.0);
        assert_eq!(v["gpu_percent"], -1.0);
        let v = parse(&stats_line(&Stats {
            fps: f64::INFINITY,
            latency_p50_ms: -7.0,
            cpu_percent: 0.0,
            gpu_percent: 250.0,
        }));
        assert_eq!(v["fps"], -1.0);
        assert_eq!(v["latency_p50_ms"], -1.0);
        assert_eq!(v["cpu_percent"], 0.0);
        assert_eq!(v["gpu_percent"], 250.0);
    }

    #[test]
    fn preview_and_frame_lines() {
        let path = "/run/user/1000/omarchy-broadcast/preview.jpg";
        let line = preview_line(Some((15, path)));
        assert_eq!(
            line,
            r#"{"v":1,"type":"preview","active":true,"fps":15,"path":"/run/user/1000/omarchy-broadcast/preview.jpg"}"#
        );
        parse(&line);
        let line = preview_line(None);
        assert_eq!(line, r#"{"v":1,"type":"preview","active":false}"#);
        parse(&line);
        let line = frame_line(7, path);
        assert_eq!(
            line,
            r#"{"v":1,"type":"frame","seq":7,"path":"/run/user/1000/omarchy-broadcast/preview.jpg"}"#
        );
        // A path is never clipped: at the bound, the lines carry all of it
        // within the line cap (`parse` checks it); one byte more does not
        // fit, and neither does a path whose escapes take it over.
        let long = format!("/{}", "d".repeat(MAX_PREVIEW_PATH_BYTES - 1));
        assert!(preview_path_fits(&long));
        let v = parse(&frame_line(u32::MAX, &long));
        assert_eq!(v["seq"], u32::MAX);
        assert_eq!(v["path"], long.as_str());
        let v = parse(&preview_line(Some((30, &long))));
        assert_eq!(v["path"], long.as_str());
        assert!(!preview_path_fits(&format!("{long}d")));
        let escaped = format!("/{}", "\u{1}".repeat(MAX_PREVIEW_PATH_BYTES / 6 + 1));
        assert!(escaped.len() < MAX_PREVIEW_PATH_BYTES);
        assert!(!preview_path_fits(&escaped));
        let v = parse(&error_line(
            CODE_PREVIEW_PATH_TOO_LONG,
            &crate::bus::preview_path_too_long(&format!("{long}d")).message(),
        ));
        assert_eq!(v["code"], "preview-path-too-long");
        // The snapshot carries the leases.
        let mut s = sample();
        s.preview_consumers = 2;
        assert_eq!(parse(&snapshot_line(&s))["preview_consumers"], 2);
        assert_eq!(
            parse(&state_line(&s, &["preview_consumers"]))["preview_consumers"],
            2
        );
    }

    #[test]
    fn error_line_has_code_and_text() {
        let v = parse(&error_line(
            CODE_NOT_RUNNING,
            "omarchy-broadcastd is not running",
        ));
        assert_eq!(v["type"], "error");
        assert_eq!(v["code"], "daemon-not-running");
        assert_eq!(v["error"], "omarchy-broadcastd is not running");
        let long = "x".repeat(10_000);
        let v = parse(&error_line(CODE_DBUS, &long));
        let text = v["error"].as_str().unwrap();
        assert!(text.ends_with(ELLIPSIS));
        assert!(text.len() <= MAX_ERROR_BYTES);
    }

    #[test]
    fn clip_bounds_the_encoded_length() {
        assert_eq!(clip("short", 10), "short");
        assert!(matches!(clip("exactly10!", 10), Cow::Borrowed(_)));
        for s in [
            "a".repeat(100),
            "\"".repeat(100),
            "\u{1}".repeat(100),
            "é".repeat(100),
            "🎥".repeat(100),
            "ab\n\t\u{7}\\\"é🎥".repeat(20),
        ] {
            for max in [4, 5, 10, 33, 64] {
                let c = clip(&s, max);
                let encoded = serde_json::to_string(c.as_ref()).unwrap();
                assert!(encoded.len() - 2 <= max, "{max}: {encoded}");
                assert!(c.ends_with(ELLIPSIS), "{c}");
                assert!(s.starts_with(c.trim_end_matches(ELLIPSIS)));
                let c = clip_start(&s, max);
                let encoded = serde_json::to_string(c.as_ref()).unwrap();
                assert!(encoded.len() - 2 <= max, "{max}: {encoded}");
                assert!(c.starts_with(ELLIPSIS), "{c}");
                assert!(s.ends_with(c.trim_start_matches(ELLIPSIS)));
            }
        }
        assert!(matches!(clip_start("exactly10!", 10), Cow::Borrowed(_)));
        assert_eq!(clip_start("/a/b/name.png", 11), "\u{2026}name.png");
        // The escape lengths agree with serde_json for every ASCII char.
        for b in 0u8..128 {
            let c = b as char;
            let encoded = serde_json::to_string(&c.to_string()).unwrap();
            assert_eq!(escaped_len(c), encoded.len() - 2, "{b:#x}");
        }
    }

    fn control(name: &str, min: i32, max: i32, step: i32, value: i32) -> CameraControl {
        CameraControl {
            name: name.into(),
            min,
            max,
            step,
            default: min,
            value,
            stored: value != min,
        }
    }

    fn controls() -> CameraControls {
        CameraControls {
            camera: "platform-video2".into(),
            controls: vec![
                control("brightness", -64, 64, 1, 10),
                control("saturation", 0, 100, 2, 0),
            ],
        }
    }

    #[test]
    fn controls_line_lists_the_controls_of_one_camera() {
        let line = controls_line(&controls());
        assert_eq!(
            line,
            r#"{"v":1,"type":"controls","camera":"platform-video2","controls":[{"name":"brightness","min":-64,"max":64,"step":1,"default":-64,"value":10,"stored":true},{"name":"saturation","min":0,"max":100,"step":2,"default":0,"value":0,"stored":false}],"controls_truncated":false}"#
        );
        parse(&line);
        // No camera: an empty id and no control.
        let v = parse(&controls_line(&CameraControls::default()));
        assert_eq!(v["camera"], "");
        assert_eq!(v["controls"], serde_json::json!([]));
        assert_eq!(v["controls_truncated"], false);
    }

    #[test]
    fn the_snapshot_carries_the_controls_or_null_from_an_older_daemon() {
        let mut s = sample();
        let v = parse(&snapshot_line(&s));
        assert_eq!(v.get("camera_controls"), Some(&Value::Null));
        s.camera_controls = Some(controls());
        s.interface_revision = 6;
        let v = parse(&snapshot_line(&s));
        let c = &v["camera_controls"];
        assert_eq!(c["camera"], "platform-video2");
        assert_eq!(c["controls_truncated"], false);
        assert_eq!(
            c["controls"][0],
            serde_json::json!({
                "name": "brightness", "min": -64, "max": 64, "step": 1,
                "default": -64, "value": 10, "stored": true
            })
        );
        assert_eq!(c["controls"].as_array().unwrap().len(), 2);
        // A `state` line does not carry them.
        assert!(
            parse(&state_line(&s, &["state"]))
                .get("camera_controls")
                .is_none()
        );
    }

    #[test]
    fn controls_are_bounded() {
        let nasty = "\u{1}".repeat(500);
        let many = CameraControls {
            camera: format!("id{nasty}"),
            controls: (0..40)
                .map(|i| {
                    control(
                        &format!("{i}{nasty}"),
                        i32::MIN,
                        i32::MAX,
                        i32::MIN,
                        i32::MIN,
                    )
                })
                .collect(),
        };
        let v = parse(&controls_line(&many));
        assert_eq!(v["controls"].as_array().unwrap().len(), MAX_CONTROLS);
        assert_eq!(v["controls_truncated"], true);
        let camera = v["camera"].as_str().unwrap();
        assert!(camera.ends_with(ELLIPSIS), "{camera}");
        assert!(serde_json::to_string(camera).unwrap().len() - 2 <= MAX_ID_BYTES);
        for c in v["controls"].as_array().unwrap() {
            let name = c["name"].as_str().unwrap();
            assert!(name.ends_with(ELLIPSIS), "{name}");
            assert!(serde_json::to_string(name).unwrap().len() - 2 <= MAX_CONTROL_NAME_BYTES);
            assert_eq!(c["min"], i32::MIN);
            assert_eq!(c["max"], i32::MAX);
        }
        // Exactly the bound: listed whole.
        let mut sixteen = many.clone();
        sixteen.controls.truncate(MAX_CONTROLS);
        assert_eq!(parse(&controls_line(&sixteen))["controls_truncated"], false);
        // A real name is printed as it is.
        let v = parse(&controls_line(&CameraControls {
            camera: "c".into(),
            controls: vec![control("backlight_compensation", 0, 2, 1, 1)],
        }));
        assert_eq!(v["controls"][0]["name"], "backlight_compensation");
    }

    #[test]
    fn a_warning_of_an_effect_failure_keeps_its_hint_under_the_cap() {
        // The daemon's longest effect failure clause: its reason at 400
        // bytes, the hint at its end, in the words `Warning` gives it.
        let reason = format!(
            "{}; see docs/setup/maxine.md, section 3",
            "x".repeat(400 - "; see docs/setup/maxine.md, section 3".len())
        );
        assert_eq!(reason.len(), 400);
        let mut s = sample();
        s.warning = format!(
            "effect replace unavailable: {reason}; the camera image is shown without the \
             effect (effect.privacy is off)"
        );
        for line in [snapshot_line(&s), state_line(&s, &model::STATE_FIELDS)] {
            let v = parse(&line);
            assert_eq!(v["warning"], s.warning.as_str(), "not clipped");
        }
    }

    #[test]
    fn worst_case_lines_stay_under_the_cap_and_valid() {
        // Every string at its worst: long and made of 6-byte escapes.
        let nasty = "\u{1}".repeat(2000);
        let mut s = sample();
        s.error = nasty.clone();
        s.warning = nasty.clone();
        s.output_mode = nasty.clone();
        s.effect = nasty.clone();
        s.background_image = nasty.clone();
        s.pipewire_output = nasty.clone();
        s.loopback_consumers = u32::MAX;
        s.pipewire_consumers = u32::MAX;
        s.preview_consumers = u32::MAX;
        // The longest number serde_json prints for a strength in range.
        s.blur_strength = 2.225_073_858_507_201_4e-308;
        s.consumers = u32::MAX;
        s.interface_revision = u32::MAX;
        s.settings_revision = Some(u32::MAX);
        s.settings_schema_serial = Some(u32::MAX);
        s.cameras = (0..100)
            .map(|i| cam(&format!("{i}{nasty}"), &nasty, &nasty))
            .collect();
        s.active_camera = s.cameras[3].id.clone();
        // The microphone (revision 13) at its worst too.
        s.mic = Some(MicStatus {
            state: nasty.clone(),
            consumers: u32::MAX,
            provider: nasty.clone(),
            latency_ms: 2.225_073_858_507_201_4e-308,
            error: nasty.clone(),
        });
        let m = parse(&mic_line(s.mic.as_ref().unwrap()));
        for key in ["state", "provider", "error"] {
            assert!(m[key].as_str().unwrap().ends_with(ELLIPSIS), "{key}");
        }
        assert!(mic_line(s.mic.as_ref().unwrap()).len() <= MAX_LINE_BYTES);
        for (line, cameras) in [
            // The settings fields (revision 7, as `interface_revision` is
            // here) and the warning (revision 11) leave no room for three
            // cameras, the mic (revision 13) for three more: the snapshot
            // cuts them.
            (snapshot_line(&s), MAX_CAMERAS - 6),
            (cameras_line(&s), MAX_CAMERAS),
            (state_line(&s, &model::STATE_FIELDS), 0),
        ] {
            let v = parse(&line);
            if let Some(cams) = v.get("cameras") {
                assert_eq!(cams.as_array().unwrap().len(), cameras, "{line}");
                assert_eq!(v["cameras_truncated"], true);
                assert_eq!(cams[3]["active"], true);
            }
            assert!(v["active_camera"].as_str().unwrap().ends_with(ELLIPSIS));
        }
        let v = parse(&snapshot_line(&s));
        // From a daemon with image controls, at their worst too: the line
        // keeps its cap. The controls are cut first, then the cameras.
        let worst_controls = CameraControls {
            camera: s.active_camera.clone(),
            controls: (0..40)
                .map(|i| {
                    control(
                        &format!("{i}{nasty}"),
                        i32::MIN,
                        i32::MAX,
                        i32::MIN,
                        i32::MIN,
                    )
                })
                .collect(),
        };
        let mut with = s.clone();
        with.camera_controls = Some(worst_controls.clone());
        let w = parse(&snapshot_line(&with));
        let c = &w["camera_controls"];
        assert_eq!(c["controls_truncated"], true, "{w}");
        assert_eq!(c["controls"], serde_json::json!([]), "{w}");
        assert!(c["camera"].as_str().unwrap().ends_with(ELLIPSIS));
        assert_eq!(w["cameras_truncated"], true);
        // The cameras are cut after the controls: with the controls gone
        // the line still has to give up some cameras.
        let kept = w["cameras"].as_array().unwrap().len();
        assert!(kept > 0 && kept < MAX_CAMERAS, "{kept} cameras kept");
        let v2 = parse(&controls_line(&worst_controls));
        assert_eq!(v2["controls"].as_array().unwrap().len(), MAX_CONTROLS);
        // With fewer cameras, the controls a daemon reports fit whole (the
        // active one alone: the mic at its worst takes the room of three).
        with.cameras = vec![with.cameras[3].clone()];
        with.camera_controls = Some(CameraControls {
            camera: with.active_camera.clone(),
            controls: obc_ipc::CAMERA_CONTROL_NAMES
                .iter()
                .map(|n| control(n, i32::MIN, i32::MAX, i32::MIN, i32::MAX))
                .collect(),
        });
        let w = parse(&snapshot_line(&with));
        assert_eq!(
            w["camera_controls"]["controls"].as_array().unwrap().len(),
            5
        );
        assert_eq!(w["camera_controls"]["controls_truncated"], false);
        assert_eq!(w["cameras"].as_array().unwrap().len(), 1);
        assert!(v["error"].as_str().unwrap().ends_with(ELLIPSIS));
        assert!(v["warning"].as_str().unwrap().ends_with(ELLIPSIS));
        assert!(v["pipewire_output"].as_str().unwrap().ends_with(ELLIPSIS));
        assert!(
            v["background_image"]
                .as_str()
                .unwrap()
                .starts_with(ELLIPSIS)
        );
        assert!(
            v["active_camera_name"]
                .as_str()
                .unwrap()
                .ends_with(ELLIPSIS)
        );
    }

    fn setting(id: &str, label: &str, kind: SettingKind) -> Setting {
        Setting {
            id: id.into(),
            label: label.into(),
            kind,
            scope: "camera".into(),
            stored: false,
            enabled: true,
            disabled_reason: String::new(),
            disabled_kind: String::new(),
            enabled_when: Vec::new(),
            hint: None,
            description: None,
        }
    }

    fn exposure_tree() -> SettingsTree {
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
                display: None,
            },
        );
        time.enabled = false;
        time.disabled_reason = "Set exposure to manual to change this".into();
        time.disabled_kind = "parent_off".into();
        time.enabled_when = vec![Condition {
            id: "camera.exposure.auto_exposure".into(),
            op: obc_ipc::ConditionOp::In,
            value: SettingValue::List(vec![
                SettingValue::Str("manual".into()),
                SettingValue::Str("shutter_priority".into()),
            ]),
        }];
        let mut mode = setting(
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
        );
        mode.hint = Some("segmented".into());
        let mut keep = setting(
            "camera.exposure.keep_framerate",
            "Keep 30 fps in low light",
            SettingKind::Bool {
                default: true,
                value: false,
            },
        );
        keep.stored = true;
        keep.hint = Some("switch".into());
        keep.description = Some("Off lets the camera lower its frame rate in low light".into());
        SettingsTree {
            revision: 12,
            serial: 3,
            camera: "usb-046d_0944".into(),
            groups: vec![
                SettingsGroup {
                    id: "exposure".into(),
                    label: "Exposure".into(),
                    settings: vec![
                        mode,
                        time,
                        keep,
                        setting(
                            "camera.exposure.future",
                            "From the future",
                            SettingKind::Unknown("colour".into()),
                        ),
                    ],
                    surface: None,
                },
                SettingsGroup {
                    id: "white_balance".into(),
                    label: "White balance".into(),
                    settings: vec![setting(
                        "camera.white_balance.do_white_balance",
                        "Balance once",
                        SettingKind::Action,
                    )],
                    surface: None,
                },
                // Nothing this client can show: no line.
                SettingsGroup {
                    id: "other".into(),
                    label: "Other".into(),
                    settings: vec![setting(
                        "camera.other.x",
                        "X",
                        SettingKind::Unknown("colour".into()),
                    )],
                    surface: None,
                },
            ],
        }
    }

    #[test]
    fn settings_lines_carry_each_group_with_the_keys_of_each_type() {
        let lines = settings_lines(&exposure_tree());
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(
            lines[0],
            r#"{"v":1,"type":"settings_group","schema":1,"rev":12,"serial":3,"camera":"usb-046d_0944","index":0,"count":2,"part":1,"parts":1,"group":{"id":"exposure","label":"Exposure","settings":[{"id":"camera.exposure.auto_exposure","label":"Exposure mode","scope":"camera","type":"enum","value":"aperture_priority","default":"aperture_priority","stored":false,"enabled":true,"disabled_reason":"","disabled_kind":"","enabled_when":[],"options":[{"value":"manual","label":"Manual"},{"value":"aperture_priority","label":"Auto"}],"hint":"segmented"},{"id":"camera.exposure.exposure_time_absolute","label":"Exposure time","scope":"camera","type":"int","value":1250,"default":156,"stored":false,"enabled":false,"disabled_reason":"Set exposure to manual to change this","disabled_kind":"parent_off","enabled_when":[{"id":"camera.exposure.auto_exposure","op":"in","value":["manual","shutter_priority"]}],"min":3,"max":2047,"step":1,"unit":"×100 µs"},{"id":"camera.exposure.keep_framerate","label":"Keep 30 fps in low light","scope":"camera","type":"bool","value":false,"default":true,"stored":true,"enabled":true,"disabled_reason":"","disabled_kind":"","enabled_when":[],"hint":"switch","description":"Off lets the camera lower its frame rate in low light"}]}}"#
        );
        assert_eq!(
            lines[1],
            r#"{"v":1,"type":"settings_group","schema":1,"rev":12,"serial":3,"camera":"usb-046d_0944","index":1,"count":2,"part":1,"parts":1,"group":{"id":"white_balance","label":"White balance","settings":[{"id":"camera.white_balance.do_white_balance","label":"Balance once","scope":"camera","type":"action","stored":false,"enabled":true,"disabled_reason":"","disabled_kind":"","enabled_when":[]}]}}"#
        );
        for line in &lines {
            parse(line);
        }
    }

    #[test]
    fn no_camera_or_nothing_to_show_is_a_clear_line() {
        let clear = r#"{"v":1,"type":"settings_clear","rev":5,"serial":2}"#;
        assert_eq!(settings_clear_line(5, 2), clear);
        let none = SettingsTree {
            revision: 5,
            serial: 2,
            camera: String::new(),
            groups: Vec::new(),
        };
        assert_eq!(settings_lines(&none), [clear]);
        // A camera without a setting, or with none of a known type.
        let mut empty = none.clone();
        empty.camera = "platform-video2".into();
        assert_eq!(settings_lines(&empty), [clear]);
        let mut unknown = exposure_tree();
        unknown.groups.drain(..2);
        unknown.revision = 5;
        unknown.serial = 2;
        assert_eq!(settings_lines(&unknown), [clear]);
        parse(clear);
    }

    #[test]
    fn global_groups_follow_the_cameras_and_show_without_a_camera() {
        let global = SettingsGroup {
            id: "video".into(),
            label: "Video".into(),
            settings: vec![Setting {
                id: "effect.video.denoise".into(),
                label: "Noise removal".into(),
                kind: SettingKind::Bool {
                    default: false,
                    value: false,
                },
                scope: obc_ipc::settings::SCOPE_GLOBAL.into(),
                stored: true,
                enabled: false,
                disabled_reason: "Not installed".into(),
                disabled_kind: "unavailable".into(),
                enabled_when: Vec::new(),
                hint: Some("switch".into()),
                description: None,
            }],
            surface: None,
        };
        let mut tree = exposure_tree();
        tree.groups.push(global.clone());
        let lines = settings_lines(&tree);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(
            lines[2].ends_with(
                r#""index":2,"count":3,"part":1,"parts":1,"group":{"id":"video","label":"Video","settings":[{"id":"effect.video.denoise","label":"Noise removal","scope":"global","type":"bool","value":false,"default":false,"stored":true,"enabled":false,"disabled_reason":"Not installed","disabled_kind":"unavailable","enabled_when":[],"hint":"switch"}]}}"#
            ),
            "{}",
            lines[2]
        );
        // No camera: the global groups alone, with an empty camera.
        let none = SettingsTree {
            revision: 5,
            serial: 2,
            camera: String::new(),
            groups: vec![global],
        };
        let lines = settings_lines(&none);
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].starts_with(
                r#"{"v":1,"type":"settings_group","schema":1,"rev":5,"serial":2,"camera":"","index":0,"count":1,"#
            ),
            "{}",
            lines[0]
        );
        parse(&lines[0]);
    }

    #[test]
    fn settings_delta_lines_carry_the_changes() {
        let changes = [
            SettingChange {
                id: "camera.exposure.auto_exposure".into(),
                value: Some(SettingValue::Str("manual".into())),
                stored: true,
                enabled: true,
                disabled_reason: String::new(),
                disabled_kind: String::new(),
            },
            SettingChange {
                id: "camera.exposure.exposure_time_absolute".into(),
                value: Some(SettingValue::Int(1250)),
                stored: false,
                enabled: true,
                disabled_reason: String::new(),
                disabled_kind: String::new(),
            },
            SettingChange {
                id: "camera.white_balance.do_white_balance".into(),
                value: None,
                stored: false,
                enabled: false,
                disabled_reason: "Not now".into(),
                disabled_kind: "busy".into(),
            },
        ];
        let lines = settings_delta_lines(13, 3, "usb-046d_0944", &changes);
        assert_eq!(
            lines,
            [
                r#"{"v":1,"type":"settings_delta","schema":1,"rev":13,"serial":3,"camera":"usb-046d_0944","changes":[{"id":"camera.exposure.auto_exposure","value":"manual","stored":true,"enabled":true,"disabled_reason":"","disabled_kind":""},{"id":"camera.exposure.exposure_time_absolute","value":1250,"stored":false,"enabled":true,"disabled_reason":"","disabled_kind":""},{"id":"camera.white_balance.do_white_balance","stored":false,"enabled":false,"disabled_reason":"Not now","disabled_kind":"busy"}]}"#
            ]
        );
        parse(&lines[0]);
        assert!(settings_delta_lines(13, 3, "c", &[]).is_empty());
        // More than a line takes: several, with the same revision, in order.
        let many: Vec<SettingChange> = (0..40)
            .map(|i| SettingChange {
                id: format!("camera.image.s{i}"),
                value: Some(SettingValue::Bool(i % 2 == 0)),
                stored: false,
                enabled: true,
                disabled_reason: String::new(),
                disabled_kind: String::new(),
            })
            .collect();
        let lines = settings_delta_lines(14, 3, "c", &many);
        assert_eq!(lines.len(), 3);
        let mut ids = Vec::new();
        for line in &lines {
            let v = parse(line);
            assert_eq!(v["rev"], 14);
            let changes = v["changes"].as_array().unwrap();
            assert!(changes.len() <= MAX_CHANGES_PER_LINE);
            ids.extend(
                changes
                    .iter()
                    .map(|c| c["id"].as_str().unwrap().to_string()),
            );
        }
        let want: Vec<String> = many.iter().map(|c| c.id.clone()).collect();
        assert_eq!(ids, want);
    }

    #[test]
    fn a_big_group_is_cut_into_parts_of_at_most_eight() {
        let settings: Vec<Setting> = (0..19)
            .map(|i| {
                setting(
                    &format!("camera.image.s{i}"),
                    "S",
                    SettingKind::Bool {
                        default: false,
                        value: true,
                    },
                )
            })
            .collect();
        let tree = SettingsTree {
            revision: 1,
            serial: 1,
            camera: "c".into(),
            groups: vec![
                SettingsGroup {
                    id: "image".into(),
                    label: "Image".into(),
                    settings,
                    surface: None,
                },
                SettingsGroup {
                    id: "focus".into(),
                    label: "Focus & zoom".into(),
                    settings: vec![setting("camera.focus.pan_reset", "P", SettingKind::Action)],
                    surface: None,
                },
            ],
        };
        let lines = settings_lines(&tree);
        let got: Vec<(u64, u64, u64, u64, String, usize)> = lines
            .iter()
            .map(|l| {
                let v = parse(l);
                (
                    v["index"].as_u64().unwrap(),
                    v["count"].as_u64().unwrap(),
                    v["part"].as_u64().unwrap(),
                    v["parts"].as_u64().unwrap(),
                    v["group"]["id"].as_str().unwrap().to_string(),
                    v["group"]["settings"].as_array().unwrap().len(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (0, 4, 1, 3, "image".into(), 8),
                (1, 4, 2, 3, "image".into(), 8),
                (2, 4, 3, 3, "image".into(), 3),
                (3, 4, 1, 1, "focus".into(), 1),
            ]
        );
        // The group's id and label are on every part.
        assert!(lines[..3].iter().all(|l| l.contains(r#""label":"Image""#)));
    }

    /// A setting with every string at its worst (long and made of 6-byte
    /// escapes), the most options and conditions, the longest numbers.
    fn worst_setting(i: usize, kind: SettingKind) -> Setting {
        let nasty = "\u{1}".repeat(500);
        // A valid id at its longest.
        let id = format!("camera.{}.s{i:0>35}", "g".repeat(4));
        assert!(obc_ipc::valid_setting_id(&id), "{id}");
        assert_eq!(id.len(), MAX_SETTING_ID_BYTES);
        let list = SettingValue::List(
            (0..40)
                .map(|_| SettingValue::Str(nasty.clone()))
                .chain([SettingValue::List(vec![SettingValue::Int(i64::MIN)])])
                .collect(),
        );
        Setting {
            id: id.clone(),
            label: nasty.clone(),
            kind,
            scope: nasty.clone(),
            stored: false,
            enabled: false,
            disabled_reason: nasty.clone(),
            disabled_kind: nasty.clone(),
            enabled_when: (0..10)
                .map(|_| Condition {
                    id: nasty.clone(),
                    op: obc_ipc::ConditionOp::Unknown(nasty.clone()),
                    value: list.clone(),
                })
                .collect(),
            hint: Some(nasty.clone()),
            description: None,
        }
    }

    fn worst_kinds() -> Vec<SettingKind> {
        let nasty = "\u{1}".repeat(500);
        vec![
            SettingKind::Enum {
                options: (0..40).map(|_| (nasty.clone(), nasty.clone())).collect(),
                default: nasty.clone(),
                value: nasty.clone(),
            },
            SettingKind::Int {
                min: i64::MIN,
                max: i64::MIN,
                step: i64::MIN,
                default: i64::MIN,
                value: i64::MIN,
                unit: Some(nasty.clone()),
                display: Some(obc_ipc::IntDisplay {
                    offset: i64::MIN,
                    scale: i64::MIN,
                }),
            },
            SettingKind::Bool {
                default: false,
                value: false,
            },
            SettingKind::Action,
        ]
    }

    #[test]
    fn worst_case_settings_lines_stay_under_the_cap_and_valid() {
        let nasty = "\u{1}".repeat(500);
        for kind in worst_kinds() {
            // A single worst-case setting fits a line alone, with the rest
            // of the line at its worst too.
            let tree = SettingsTree {
                revision: u32::MAX,
                serial: u32::MAX,
                camera: nasty.clone(),
                groups: vec![SettingsGroup {
                    id: nasty.clone(),
                    label: nasty.clone(),
                    settings: vec![worst_setting(0, kind.clone())],
                    // Revision 13's surface at its worst too.
                    surface: Some(nasty.clone()),
                }],
            };
            let lines = settings_lines(&tree);
            assert_eq!(lines.len(), 1, "{kind:?}");
            let v = parse(&lines[0]);
            assert_eq!(v["type"], "settings_group");
            assert!(
                v["group"]["surface"].as_str().unwrap().ends_with(ELLIPSIS),
                "{kind:?}"
            );
            let s = &v["group"]["settings"][0];
            assert!(s["label"].as_str().unwrap().ends_with(ELLIPSIS));
            assert!(s["disabled_reason"].as_str().unwrap().len() <= MAX_REASON_BYTES);
            let kind = s["disabled_kind"].as_str().unwrap();
            assert!(
                kind.ends_with(ELLIPSIS) && kind.len() <= MAX_DISABLED_KIND_BYTES,
                "{kind:?}"
            );
            // The scope too: clipped to its bound, never left out.
            let scope = s["scope"].as_str().unwrap();
            assert!(scope.ends_with(ELLIPSIS), "{scope:?}");
            assert!(
                serde_json::to_string(scope).unwrap().len() - 2 <= MAX_SCOPE_BYTES,
                "{scope:?}"
            );
            let when = s["enabled_when"].as_array().unwrap();
            assert_eq!(when.len(), MAX_CONDITIONS);
            let operands = when[0]["value"].as_array().unwrap();
            assert_eq!(operands.len(), MAX_CONDITION_VALUES);
            if let Some(options) = s.get("options") {
                assert_eq!(options.as_array().unwrap().len(), MAX_OPTIONS);
            }
        }
        // Many of them: every line valid and under the cap, no setting lost.
        let groups: Vec<SettingsGroup> = (0..5)
            .map(|g| SettingsGroup {
                id: nasty.clone(),
                label: nasty.clone(),
                settings: (0..64)
                    .map(|i| worst_setting(g * 64 + i, worst_kinds()[i % 4].clone()))
                    .collect(),
                surface: None,
            })
            .collect();
        let tree = SettingsTree {
            revision: u32::MAX,
            serial: u32::MAX,
            camera: nasty.clone(),
            groups,
        };
        let lines = settings_lines(&tree);
        let mut seen = 0;
        for (i, line) in lines.iter().enumerate() {
            let v = parse(line);
            assert_eq!(v["index"], i);
            assert_eq!(v["count"], lines.len());
            let n = v["group"]["settings"].as_array().unwrap().len();
            assert!((1..=MAX_SETTINGS_PER_LINE).contains(&n), "{n}");
            seen += n;
        }
        assert_eq!(seen, 5 * 64);
        // The deltas likewise.
        let changes: Vec<SettingChange> = (0..100)
            .map(|i| SettingChange {
                id: worst_setting(i, SettingKind::Action).id,
                value: Some(match i % 3 {
                    0 => SettingValue::Str(nasty.clone()),
                    1 => SettingValue::Int(i64::MIN),
                    _ => SettingValue::List(
                        (0..40).map(|_| SettingValue::Str(nasty.clone())).collect(),
                    ),
                }),
                stored: false,
                enabled: false,
                disabled_reason: nasty.clone(),
                disabled_kind: nasty.clone(),
            })
            .collect();
        let lines = settings_delta_lines(u32::MAX, u32::MAX, &nasty, &changes);
        let mut seen = 0;
        for line in &lines {
            let v = parse(line);
            let n = v["changes"].as_array().unwrap().len();
            assert!((1..=MAX_CHANGES_PER_LINE).contains(&n), "{n}");
            seen += n;
        }
        assert_eq!(seen, 100);
        parse(&settings_clear_line(u32::MAX, u32::MAX));
    }

    #[test]
    fn the_snapshot_carries_the_settings_revision_and_schema_or_null() {
        let mut s = sample();
        s.settings_revision = Some(9);
        s.settings_schema_serial = Some(2);
        // From a daemon too old for them: null, with the keys present.
        s.interface_revision = 6;
        let v = parse(&snapshot_line(&s));
        assert_eq!(v.get("settings_revision"), Some(&Value::Null));
        assert_eq!(v.get("settings_schema"), Some(&Value::Null));
        s.interface_revision = obc_ipc::SETTINGS_REVISION;
        let v = parse(&snapshot_line(&s));
        assert_eq!(v["settings_revision"], 9);
        assert_eq!(v["settings_schema"], SETTINGS_SCHEMA);
        assert_eq!(v["settings_schema"], 1);
        // Not in a `state` line.
        let v = parse(&state_line(&s, &["state"]));
        assert!(v.get("settings_revision").is_none());
        assert!(v.get("settings_schema").is_none());
    }

    #[test]
    fn the_mic_line_and_the_snapshots_mic() {
        let m = MicStatus {
            state: "active".into(),
            consumers: 1,
            provider: "p".repeat(400),
            latency_ms: f64::NAN,
            error: String::new(),
        };
        let v = parse(&mic_line(&m));
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "mic");
        assert_eq!(v["state"], "active");
        assert_eq!(v["consumers"], 1);
        assert_eq!(v["latency_ms"], 0.0);
        assert_eq!(v["error"], "");
        assert!(v["provider"].as_str().unwrap().len() <= MAX_MIC_PROVIDER_BYTES);
        let mut s = sample();
        assert_eq!(parse(&snapshot_line(&s))["mic"], Value::Null);
        s.mic = Some(MicStatus::default());
        let v = parse(&snapshot_line(&s));
        assert_eq!(v["mic"]["state"], "off");
        assert_eq!(v["mic"]["consumers"], 0);
    }

    #[test]
    fn an_int_carries_its_display_transform_when_it_has_one() {
        let int = |display| {
            setting(
                "camera.white_balance.temperature_compensation",
                "Temperature compensation",
                SettingKind::Int {
                    min: 0,
                    max: 100,
                    step: 1,
                    default: 50,
                    value: 57,
                    unit: Some("K".into()),
                    display,
                },
            )
        };
        let tree = |s| SettingsTree {
            revision: 1,
            serial: 1,
            camera: "usb-046d_0944".into(),
            groups: vec![SettingsGroup {
                id: "white_balance".into(),
                label: "White balance".into(),
                settings: vec![s],
                surface: None,
            }],
        };
        let lines = settings_lines(&tree(int(Some(obc_ipc::IntDisplay {
            offset: 50,
            scale: 40,
        }))));
        let v = parse(&lines[0]);
        let s = &v["group"]["settings"][0];
        assert_eq!(s["display_offset"], 50);
        assert_eq!(s["display_scale"], 40);
        // The numbers stay raw.
        assert_eq!(
            (s["value"].clone(), s["default"].clone()),
            (57.into(), 50.into())
        );
        assert_eq!((s["min"].clone(), s["max"].clone()), (0.into(), 100.into()));
        assert_eq!(s["unit"], "K");
        assert!(lines[0].contains(r#""unit":"K","display_offset":50,"display_scale":40"#));
        // None: neither key.
        let lines = settings_lines(&tree(int(None)));
        let v = parse(&lines[0]);
        let s = &v["group"]["settings"][0];
        assert!(s.get("display_offset").is_none() && s.get("display_scale").is_none());
    }

    #[test]
    fn a_settings_group_line_carries_its_surface_when_it_has_one() {
        let group = |surface: Option<&str>| SettingsGroup {
            id: "noise".into(),
            label: "Noise removal".into(),
            settings: vec![Setting {
                id: "mic.noise.enabled".into(),
                label: "Noise removal".into(),
                kind: SettingKind::Bool {
                    default: true,
                    value: true,
                },
                scope: "global".into(),
                stored: false,
                enabled: true,
                disabled_reason: String::new(),
                disabled_kind: String::new(),
                enabled_when: Vec::new(),
                hint: None,
                description: None,
            }],
            surface: surface.map(str::to_string),
        };
        let tree = |g| SettingsTree {
            revision: 1,
            serial: 1,
            camera: String::new(),
            groups: vec![g],
        };
        let lines = settings_lines(&tree(group(Some("mic"))));
        assert_eq!(parse(&lines[0])["group"]["surface"], "mic");
        let lines = settings_lines(&tree(group(None)));
        assert!(parse(&lines[0])["group"].get("surface").is_none());
    }

    fn health() -> CameraHealth {
        CameraHealth {
            camera: "usb-1234_abcd".into(),
            delivered_fps: 19.5,
            nominal_fps: 30.0,
            corrupt_frames: 7,
            warnings: vec!["Plug it into its own USB port.".into()],
            suggestions: vec![obc_ipc::Suggestion {
                setting: "effect.video.denoise".into(),
                value: "on".into(),
                reason: "This camera's picture is grainy.".into(),
            }],
            has_recommendation: true,
            recommended: vec![obc_ipc::Recommended {
                setting: "camera.image.sharpness".into(),
                label: "Sharpness".into(),
                value: "5".into(),
                current: "3".into(),
            }],
            shutter: obc_ipc::SHUTTER_CLOSED.into(),
            show_mode: obc_ipc::SHOW_MODE_DESK_VIEW.into(),
            status: vec!["Shutter closed".into(), "Show Mode: desk view".into()],
        }
    }

    #[test]
    fn the_camera_health_line_and_the_snapshots_camera_health() {
        let h = health();
        let v = parse(&camera_health_line(&h));
        assert_eq!(
            v,
            serde_json::json!({
                "v": 1,
                "type": "camera_health",
                "camera": "usb-1234_abcd",
                "delivered_fps": 19.5,
                "nominal_fps": 30.0,
                "corrupt_frames": 7,
                "warnings": ["Plug it into its own USB port."],
                "suggestions": [{
                    "setting": "effect.video.denoise",
                    "value": "on",
                    "reason": "This camera's picture is grainy."
                }],
                "has_recommendation": true,
                "recommended": [{
                    "setting": "camera.image.sharpness",
                    "label": "Sharpness",
                    "value": "5",
                    "current": "3"
                }],
                "shutter": "closed",
                "show_mode": "desk_view",
                "status": ["Shutter closed", "Show Mode: desk view"]
            })
        );
        // An older daemon's (no camera status keys): empty, the keys there.
        let v = parse(&camera_health_line(&CameraHealth {
            shutter: String::new(),
            show_mode: String::new(),
            status: Vec::new(),
            ..h.clone()
        }));
        assert_eq!(
            (&v["shutter"], &v["show_mode"], &v["status"]),
            (
                &Value::from(""),
                &Value::from(""),
                &Value::Array(Vec::new())
            )
        );
        // Before revision 20: null, the key present.
        let mut s = sample();
        let v = parse(&snapshot_line(&s));
        assert_eq!(v.get("camera_health"), Some(&Value::Null));
        s.camera_health = Some(h.clone());
        // Served by a daemon below revision 20: still null.
        s.interface_revision = 19;
        assert_eq!(parse(&snapshot_line(&s))["camera_health"], Value::Null);
        s.interface_revision = obc_ipc::CAMERA_HEALTH_REVISION;
        let v = parse(&snapshot_line(&s));
        assert_eq!(v["camera_health"]["delivered_fps"], 19.5);
        assert_eq!(v["camera_health"]["recommended"][0]["current"], "3");
        // Not measured, and a rate that is no number: -1.
        let mut none = CameraHealth::none();
        none.nominal_fps = f64::NAN;
        let v = parse(&camera_health_line(&none));
        assert_eq!(v["delivered_fps"], -1.0);
        assert_eq!(v["nominal_fps"], -1.0);
        assert_eq!(v["camera"], "");
        // Never in a state line.
        assert!(
            parse(&state_line(&s, &model::STATE_FIELDS))
                .get("camera_health")
                .is_none()
        );
    }

    #[test]
    fn the_camera_health_at_its_worst_stays_under_the_cap() {
        let nasty = "\u{1}".repeat(2000);
        let h = CameraHealth {
            camera: nasty.clone(),
            delivered_fps: 2.225_073_858_507_201_4e-308,
            nominal_fps: 2.225_073_858_507_201_4e-308,
            corrupt_frames: u64::MAX,
            warnings: vec![nasty.clone(); 10],
            suggestions: vec![
                obc_ipc::Suggestion {
                    setting: nasty.clone(),
                    value: nasty.clone(),
                    reason: nasty.clone(),
                };
                10
            ],
            has_recommendation: true,
            recommended: vec![
                obc_ipc::Recommended {
                    setting: nasty.clone(),
                    label: nasty.clone(),
                    value: nasty.clone(),
                    current: nasty.clone(),
                };
                40
            ],
            shutter: nasty.clone(),
            show_mode: nasty.clone(),
            status: vec![nasty.clone(); 10],
        };
        let line = camera_health_line(&h);
        assert!(line.len() <= MAX_LINE_BYTES, "{} bytes", line.len());
        let v = parse(&line);
        // The recommended changes go first; the warnings stay whole.
        assert_eq!(v["warnings"].as_array().unwrap().len(), MAX_HEALTH_WARNINGS);
        assert_eq!(v["suggestions"].as_array().unwrap().len(), MAX_SUGGESTIONS);
        assert!(v["recommended"].as_array().unwrap().len() < MAX_RECOMMENDED);
        assert!(v["warnings"][0].as_str().unwrap().ends_with(ELLIPSIS));
        assert!(v["camera"].as_str().unwrap().ends_with(ELLIPSIS));
        assert!(v["shutter"].as_str().unwrap().ends_with(ELLIPSIS));
        assert!(v["show_mode"].as_str().unwrap().ends_with(ELLIPSIS));
        assert!(v["status"].as_array().unwrap().len() <= MAX_HEALTH_STATUS);
        // In a snapshot at its worst: under the cap, the health's lists cut
        // before the cameras.
        let mut s = sample();
        s.error = nasty.clone();
        s.warning = nasty.clone();
        s.cameras = (0..100)
            .map(|i| cam(&format!("{i}{nasty}"), &nasty, &nasty))
            .collect();
        s.camera_health = Some(h);
        s.interface_revision = obc_ipc::CAMERA_HEALTH_REVISION;
        let line = snapshot_line(&s);
        assert!(line.len() <= MAX_LINE_BYTES, "{} bytes", line.len());
        let v = parse(&line);
        assert!(!v["cameras"].as_array().unwrap().is_empty());
        assert!(
            v["camera_health"]["recommended"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        // A short health is kept whole beside the usual cameras.
        let mut s = sample();
        s.camera_health = Some(health());
        s.interface_revision = obc_ipc::CAMERA_HEALTH_REVISION;
        let v = parse(&snapshot_line(&s));
        assert_eq!(
            v["camera_health"]["recommended"].as_array().unwrap().len(),
            1
        );
    }
}
