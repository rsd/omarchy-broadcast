//! The camera's health (D-Bus revision 20, `CameraHealth`), as pure rules
//! the controller feeds: the warnings about a camera's USB placement and
//! its damaged frames, the effect suggestions, the recommended settings of
//! its model, the label of a capture mode that delivers short of its rate,
//! and the mains frequency of the time zone.
//!
//! - **Shared hub** ([`shared_hub_warning`]): a camera below a hub that
//!   also carries keyboards or mice, streaming a raw mode of at least
//!   [`HUB_HEAVY_BYTES_PER_SECOND`], takes most of the hub's bandwidth: the
//!   HID devices stall and frames come damaged. A Microsoft LifeCam VX-800
//!   in YUYV 640x480 at 30 fps (18.4 MB/s) on a hub with a keyboard and a
//!   mouse reset them and lost 14 to 22 % of its frames; a Logitech C930e
//!   in MJPEG on the same hub lost none. MJPEG has no fixed rate and never
//!   counts.
//! - **Damaged frames** ([`DamageWindow`], [`damaged_warning`]): at least
//!   [`DAMAGE_RATIO`] of the frames dropped as damaged over the last
//!   [`DAMAGE_WINDOW`], whatever the topology; it goes once the share is
//!   below [`DAMAGE_CLEAR_RATIO`] ([`damaged_now`]).
//! - **Suggestions** ([`suggestions`]): noise removal for a model its row
//!   calls noisy, enhancement for a camera whose largest mode is below 720
//!   lines; never one whose setting already has that value or is greyed
//!   (its stage cannot run here). Never the HDR look.
//! - **Recommended settings** ([`recommended_targets`], [`recommended`]):
//!   every device setting of the camera at its default, overlaid with its
//!   model's row ([`obc_pipeline::quirks::profile`]) and the power-line
//!   frequency of the time zone; listed where it differs from now.
//! - **Capture-mode label** ([`shown_rate`], [`mark_delivered`]): the mode
//!   in use reads `640x480 · 19.5 of 30 fps` once it delivers below
//!   [`LABEL_ON_RATIO`] of its rate, until it is back above
//!   [`LABEL_OFF_RATIO`]; the rate shown moves by [`LABEL_FPS_STEP`].
//! - **Mains frequency** ([`mains_hz`]): 50 or 60 Hz by time zone, `None`
//!   where a country has both (Japan) or the zone says nothing.

use std::collections::VecDeque;
use std::path::Path;
use std::time::{Duration, Instant};

use obc_core::VideoMode;
use obc_ipc::{Recommended, SettingKind, SettingValue, Suggestion};
use obc_pipeline::usb_topology::{SharedHub, UsbPlacement, raw_bytes_per_second};

use crate::settings::catalogue::DYNAMIC_FRAMERATE;
use crate::settings::effects::GlobalEntry;
use crate::settings::{CameraSettings, Entry, Target, Values};

/// A raw stream of at least this many bytes per second on a hub shared
/// with HID devices draws the shared-hub warning: well above what a
/// High-Speed hub carries for a keyboard and a mouse to stay responsive
/// (the VX-800's 640x480 YUYV at 30 fps is 18.4 MB/s).
pub const HUB_HEAVY_BYTES_PER_SECOND: u64 = 12_000_000;

/// The span the damaged-frame share is measured over.
pub const DAMAGE_WINDOW: Duration = Duration::from_secs(10);
/// The shortest span a damaged-frame share is judged on.
pub const DAMAGE_MIN_SPAN: Duration = Duration::from_secs(5);
/// The share of damaged frames that draws the damaged-frames warning.
pub const DAMAGE_RATIO: f64 = 0.05;
/// The share of damaged frames below which a warning drawn goes: below
/// [`DAMAGE_RATIO`], so that a share hovering at the mark does not make it
/// come and go.
pub const DAMAGE_CLEAR_RATIO: f64 = 0.03;

/// The delivered rate below which, as a share of the mode's rate, the
/// capture mode's label shows it ([`obc_ipc::DELIVERS_SHORT_RATIO`]).
pub const LABEL_ON_RATIO: f64 = obc_ipc::DELIVERS_SHORT_RATIO;
/// The delivered rate above which, as a share of the mode's rate, a label
/// that shows it goes back to the mode's rate alone: above
/// [`LABEL_ON_RATIO`], so that a rate hovering at the mark does not make
/// the label come and go.
pub const LABEL_OFF_RATIO: f64 = 0.97;
/// The move of the delivered rate that changes the rate a label shows. A
/// label change is a structural change of the settings tree (clients read
/// it again), so the label does not follow every tenth: a rate is shown to
/// 0.1 fps (as `CameraHealth` publishes it) and moved only by this much.
pub const LABEL_FPS_STEP: f64 = 1.5;

/// The noise removal switch.
pub const DENOISE: &str = obc_core::modules::legacy::DENOISE;
/// The enhancement switch.
pub const ENHANCE: &str = obc_core::modules::legacy::ENHANCE;
/// Below this many lines, a camera's picture gains from enhancement.
pub const ENHANCE_BELOW_LINES: u32 = 720;

/// The setting of the power-line frequency (the storage key).
pub const POWER_LINE: &str = "power_line_frequency";

/// The time zones whose mains run at 60 Hz. Sources: the IEC's World
/// Plugs list and the "Mains electricity by country" tables (2026). The
/// Americas are 60 Hz but for the zones of [`AMERICAS_50_HZ`] and
/// [`MIXED_OR_UNKNOWN`]; outside them only these zones are.
const ZONES_60_HZ: &[&str] = &[
    // Asia.
    "Asia/Taipei",
    "Asia/Seoul",
    "Asia/Pyongyang",
    "Asia/Manila",
    "Asia/Riyadh",
    // Africa: Liberia.
    "Africa/Monrovia",
    // The Atlantic: Bermuda.
    "Atlantic/Bermuda",
    // The Pacific: Hawaii, the US territories and the states in free
    // association with the US, French Polynesia, the Galápagos.
    "Pacific/Honolulu",
    "Pacific/Johnston",
    "Pacific/Midway",
    "Pacific/Wake",
    "Pacific/Guam",
    "Pacific/Saipan",
    "Pacific/Pago_Pago",
    "Pacific/Samoa",
    "Pacific/Majuro",
    "Pacific/Kwajalein",
    "Pacific/Palau",
    "Pacific/Chuuk",
    "Pacific/Truk",
    "Pacific/Yap",
    "Pacific/Pohnpei",
    "Pacific/Ponape",
    "Pacific/Kosrae",
    "Pacific/Tahiti",
    "Pacific/Marquesas",
    "Pacific/Gambier",
    "Pacific/Galapagos",
];

/// The `America/` zones whose mains run at 50 Hz: Argentina, Chile,
/// Uruguay, Paraguay, Bolivia, French Guiana, Greenland, Saint Pierre and
/// Miquelon, and the 50 Hz islands of the Caribbean.
const AMERICAS_50_HZ: &[&str] = &[
    "Argentina/",
    "Buenos_Aires",
    "Catamarca",
    "Cordoba",
    "Jujuy",
    "Mendoza",
    "Rosario",
    "Santiago",
    "Punta_Arenas",
    "Coyhaique",
    "Montevideo",
    "Asuncion",
    "La_Paz",
    "Cayenne",
    "Nuuk",
    "Godthab",
    "Scoresbysund",
    "Danmarkshavn",
    "Miquelon",
    "Jamaica",
    "Barbados",
    "Dominica",
    "Grenada",
    "St_Lucia",
    "St_Vincent",
    "Martinique",
    "Guadeloupe",
    "Curacao",
    "Kralendijk",
];

/// Zones with both frequencies, or where the sources disagree: nothing is
/// written. Japan has two grids (50 Hz in the east, 60 Hz in the west);
/// Guyana runs both.
///
/// The tables list no slash-less legacy zone (`Japan`, `Jamaica`):
/// [`zone_name`] only names zones with an area (`Asia/Tokyo`), so none of
/// them reaches [`mains_hz`] from the system's zone, and one passed
/// directly places nothing. (`Jamaica` in [`AMERICAS_50_HZ`] is the city of
/// `America/Jamaica`.)
const MIXED_OR_UNKNOWN: &[&str] = &[
    "Asia/Tokyo",
    "America/Guyana",
    "America/Thule",
    "America/Marigot",
    "America/St_Barthelemy",
];

/// Areas whose zones (other than those listed above) are all 50 Hz.
const AREAS_50_HZ: &[&str] = &[
    "Europe/",
    "Africa/",
    "Asia/",
    "Australia/",
    "Indian/",
    "Atlantic/",
    "Arctic/",
];

/// The `Pacific/` zones at 50 Hz (the others are 60 Hz or unknown).
const PACIFIC_50_HZ: &[&str] = &[
    "Auckland",
    "Chatham",
    "Fiji",
    "Noumea",
    "Tongatapu",
    "Apia",
    "Port_Moresby",
    "Bougainville",
    "Guadalcanal",
    "Efate",
    "Tarawa",
    "Norfolk",
    "Rarotonga",
    "Niue",
    "Funafuti",
    "Nauru",
    "Wallis",
    "Easter",
];

/// The mains frequency of the time zone `zone` (`Europe/Lisbon`), in Hz:
/// 50 or 60, `None` for a zone with both (Japan) or one the tables do not
/// place (`UTC`, Antarctica, a legacy alias they do not list).
pub fn mains_hz(zone: &str) -> Option<u32> {
    let zone = zone.trim();
    if MIXED_OR_UNKNOWN.contains(&zone) {
        return None;
    }
    if ZONES_60_HZ.contains(&zone) {
        return Some(60);
    }
    if let Some(city) = zone.strip_prefix("America/") {
        let fifty = AMERICAS_50_HZ
            .iter()
            .any(|z| city == *z || (z.ends_with('/') && city.starts_with(z)));
        return Some(if fifty { 50 } else { 60 });
    }
    // Legacy aliases of the 60 Hz countries of the Americas.
    for prefix in ["US/", "Canada/", "Mexico/", "Brazil/"] {
        if zone.starts_with(prefix) {
            return Some(60);
        }
    }
    if zone.starts_with("Chile/") {
        return Some(50);
    }
    if let Some(city) = zone.strip_prefix("Pacific/") {
        return PACIFIC_50_HZ.contains(&city).then_some(50);
    }
    if AREAS_50_HZ.iter().any(|a| zone.starts_with(a)) {
        return Some(50);
    }
    None
}

/// The zone named by a `TZ` value (`Europe/Lisbon`, `:Europe/Lisbon`, a
/// path into the zoneinfo database), or by the target of the
/// `/etc/localtime` link (`../usr/share/zoneinfo/Europe/Lisbon`); `None`
/// for a POSIX rule (`EST5EDT`), an empty value or a path outside the
/// database.
pub fn zone_name(value: &str) -> Option<String> {
    let v = value.trim().trim_start_matches(':');
    let v = match v.rfind("zoneinfo/") {
        Some(i) => &v[i + "zoneinfo/".len()..],
        None if v.starts_with('/') || v.starts_with('.') => return None,
        None => v,
    };
    let v = v
        .strip_prefix("posix/")
        .or_else(|| v.strip_prefix("right/"))
        .unwrap_or(v);
    let ok = !v.is_empty()
        && v.contains('/')
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '+'));
    ok.then(|| v.to_string())
}

/// The local time zone's name: `TZ`, else the target of `/etc/localtime`.
pub fn local_zone() -> Option<String> {
    if let Ok(tz) = std::env::var("TZ")
        && !tz.trim().is_empty()
    {
        return zone_name(&tz);
    }
    let target = std::fs::read_link(Path::new("/etc/localtime")).ok()?;
    zone_name(target.to_str()?)
}

/// `a`, `a and b`, `a, b and c`; at most three names, then `and N more`.
fn names_list(names: &[String]) -> String {
    const SHOWN: usize = 3;
    let mut shown: Vec<String> = names.iter().take(SHOWN).cloned().collect();
    if names.len() > SHOWN {
        shown.push(format!("{} more", names.len() - SHOWN));
    }
    match shown.len() {
        0 => "other devices".into(),
        1 => shown.remove(0),
        n => format!("{} and {}", shown[..n - 1].join(", "), shown[n - 1]),
    }
}

/// A raw mode in words: `uncompressed 640x480 at 30 fps`.
fn mode_words(mode: &VideoMode) -> String {
    let fps = mode.fps();
    let fps = if (fps - fps.round()).abs() < 0.005 {
        format!("{}", fps.round() as i64)
    } else {
        format!("{fps:.2}")
    };
    format!("uncompressed {}x{} at {fps} fps", mode.width, mode.height)
}

/// The hub `placement` shares with HID devices, if it does and names any.
fn hid_hub(placement: Option<&UsbPlacement>) -> Option<&SharedHub> {
    placement?
        .shared_hub
        .as_ref()
        .filter(|h| !h.hid_devices.is_empty())
}

/// The shared-hub warning of the camera `name` in `mode` at `placement`
/// (see the module docs); `None` when it does not apply.
pub fn shared_hub_warning(
    name: &str,
    placement: Option<&UsbPlacement>,
    mode: &VideoMode,
) -> Option<String> {
    let hub = hid_hub(placement)?;
    let rate = raw_bytes_per_second(mode)?;
    (rate >= HUB_HEAVY_BYTES_PER_SECOND).then(|| {
        format!(
            "Plug {name} into its own USB port: it shares a hub with {}, and its {} stream uses \
             most of the hub's bandwidth, which can stall the keyboard or mouse and damage \
             frames.",
            names_list(&hub.hid_devices),
            mode_words(mode)
        )
    })
}

/// The damaged-frames warning of the camera `name`; with the hub advice
/// when it shares one with HID devices (`hub`).
pub fn damaged_warning(name: &str, hub: Option<&UsbPlacement>) -> String {
    let mut text = format!(
        "{name} sends damaged frames: at least 1 in 20 of its frames were dropped in the last \
         10 seconds."
    );
    match hid_hub(hub) {
        Some(h) => text.push_str(&format!(
            " Plug it into its own USB port: it shares a hub with {}.",
            names_list(&h.hid_devices)
        )),
        None => text.push_str(" Try another USB port or cable."),
    }
    text
}

/// Whether a damaged-frames warning should carry the hub advice: the
/// shared-hub warning does not already say it.
pub fn hub_advice_needed(placement: Option<&UsbPlacement>, mode: &VideoMode) -> bool {
    hid_hub(placement).is_some() && shared_hub_warning("", placement, mode).is_none()
}

/// Whether a camera's stream counts as damaged at the share `ratio`, given
/// whether it did (`was`): from [`DAMAGE_RATIO`] on, and once so until the
/// share is below [`DAMAGE_CLEAR_RATIO`].
pub fn damaged_now(was: bool, ratio: f64) -> bool {
    if was {
        ratio >= DAMAGE_CLEAR_RATIO
    } else {
        ratio >= DAMAGE_RATIO
    }
}

/// The damaged-frame share of one camera's stream over the last
/// [`DAMAGE_WINDOW`]: samples of the guard's count of damaged frames and
/// of the good frames delivered (their rate times the time between
/// samples).
#[derive(Debug, Clone, Default)]
pub struct DamageWindow {
    /// `(when, damaged so far, good so far)`, oldest first.
    samples: VecDeque<(Instant, u64, f64)>,
}

impl DamageWindow {
    /// A new stream: nothing measured.
    pub fn clear(&mut self) {
        self.samples.clear();
    }

    /// Records, at `now`, the damaged frames counted since the stream
    /// started (`damaged`) and the rate good frames arrive at (`fps`).
    pub fn record(&mut self, now: Instant, damaged: u64, fps: f64) {
        let fps = if fps.is_finite() && fps > 0.0 {
            fps
        } else {
            0.0
        };
        let good = match self.samples.back() {
            Some((t, _, g)) => {
                // A gap (the stream paused) counts at most one window.
                let dt = now.saturating_duration_since(*t).min(DAMAGE_WINDOW);
                g + fps * dt.as_secs_f64()
            }
            None => 0.0,
        };
        self.samples.push_back((now, damaged, good));
        // Keep one sample at or before the window's start as its base.
        while self.samples.len() > 2
            && self
                .samples
                .get(1)
                .is_some_and(|(t, _, _)| now.saturating_duration_since(*t) >= DAMAGE_WINDOW)
        {
            self.samples.pop_front();
        }
    }

    /// The share of damaged frames over the window, once it spans
    /// [`DAMAGE_MIN_SPAN`]; `None` before, and with no frame at all.
    pub fn ratio(&self) -> Option<f64> {
        let (t0, d0, g0) = *self.samples.front()?;
        let (t1, d1, g1) = *self.samples.back()?;
        if t1.saturating_duration_since(t0) < DAMAGE_MIN_SPAN {
            return None;
        }
        let damaged = d1.saturating_sub(d0) as f64;
        let total = damaged + (g1 - g0).max(0.0);
        (total > 0.0).then(|| damaged / total)
    }
}

/// The global setting `id`, if it is in the tree.
fn global<'a>(entries: &'a [GlobalEntry], id: &str) -> Option<&'a GlobalEntry> {
    entries.iter().find(|e| e.setting.id == id)
}

/// Whether switching the global setting `id` on is worth suggesting: it is
/// in the tree, enabled (its stage can run here) and off.
fn can_turn_on(entries: &[GlobalEntry], id: &str) -> bool {
    global(entries, id).is_some_and(|e| {
        e.setting.enabled && e.setting.kind.value() == Some(SettingValue::Bool(false))
    })
}

/// The effect suggestions for a camera whose model is `noisy` and whose
/// largest mode has `lines` lines (`None`: not known), against the global
/// settings `entries` as the tree publishes them.
pub fn suggestions(noisy: bool, lines: Option<u32>, entries: &[GlobalEntry]) -> Vec<Suggestion> {
    let mut out = Vec::new();
    if noisy && can_turn_on(entries, DENOISE) {
        out.push(Suggestion {
            setting: DENOISE.into(),
            value: "on".into(),
            reason: "This camera's picture is grainy: noise removal makes it cleaner.".into(),
        });
    }
    if let Some(lines) = lines.filter(|l| *l < ENHANCE_BELOW_LINES)
        && can_turn_on(entries, ENHANCE)
    {
        out.push(Suggestion {
            setting: ENHANCE.into(),
            value: "on".into(),
            reason: format!(
                "This camera captures at most {lines} lines: enhancement makes its picture \
                 sharper."
            ),
        });
    }
    out
}

/// The raw value of the option `id` of an enum entry.
fn option_raw(e: &Entry, id: &str) -> Option<i32> {
    match &e.values {
        Values::Options(o) => o.iter().find(|(o, _)| o == id).map(|(_, raw)| *raw),
        _ => None,
    }
}

/// The raw power-line value for `hz` on `e` (its `50hz` or `60hz`
/// option), if the camera offers it.
pub fn mains_raw(e: &Entry, hz: u32) -> Option<i32> {
    let raw = option_raw(e, &format!("{hz}hz"))?;
    e.check_raw(raw).is_ok().then_some(raw)
}

/// Whether the reset of the camera writes `e` (see
/// `Controller::write_defaults`): a value with a default the camera lets
/// change now.
fn reset_writes(e: &Entry) -> bool {
    use obc_pipeline::controls::{CTRL_FLAG_DISABLED, CTRL_FLAG_READ_ONLY};
    !e.is_action()
        && e.default.is_some()
        && !e.inactive()
        && e.flags & (CTRL_FLAG_READ_ONLY | CTRL_FLAG_DISABLED) == 0
}

/// What "Use recommended settings" sets each device setting of `model` to,
/// as `(index, raw)`: the default, or the row's value (`row`, by V4L2
/// control id), or for the power-line frequency the value of `mains` Hz
/// when the camera offers it. Only the settings the reset writes; a row
/// value the camera does not take is left out (the default stays). While
/// the reset meets the frame rate pin (`pinned`: `[camera]
/// fixed_framerate`), it puts `exposure_dynamic_framerate` back to the
/// pin's 0, not to its default (see `Controller::write_defaults`): that 0
/// is its target, unless the row sets it.
pub fn recommended_targets(
    model: &CameraSettings,
    row: &[(u32, i64)],
    mains: Option<u32>,
    pinned: bool,
) -> Vec<(usize, i32)> {
    model
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| reset_writes(e))
        .filter_map(|(i, e)| {
            let default = e.default?;
            let from_row = row
                .iter()
                .find(|(cid, _)| e.target == Target::Ctrl(*cid))
                .and_then(|(_, v)| i32::try_from(*v).ok())
                .filter(|v| e.check_raw(*v).is_ok());
            let from_zone = (e.name == POWER_LINE)
                .then(|| mains.and_then(|hz| mains_raw(e, hz)))
                .flatten();
            let reset = if pinned && e.name == DYNAMIC_FRAMERATE {
                0
            } else {
                default
            };
            Some((i, from_row.or(from_zone).unwrap_or(reset)))
        })
        .collect()
}

/// The values of the row and of the zone alone (what is written, and
/// stored, after the reset), as `(index, raw)`.
pub fn recommended_writes(
    model: &CameraSettings,
    row: &[(u32, i64)],
    mains: Option<u32>,
    pinned: bool,
) -> Vec<(usize, i32)> {
    recommended_targets(model, row, mains, pinned)
        .into_iter()
        .filter(|(i, raw)| {
            let e = &model.entries[*i];
            let in_row = row.iter().any(|(cid, _)| e.target == Target::Ctrl(*cid));
            in_row || (e.name == POWER_LINE && mains.and_then(|hz| mains_raw(e, hz)) == Some(*raw))
        })
        .collect()
}

/// `value` of a setting of `kind` as the settings tree displays it: a
/// number with its unit, `On` or `Off`, an option's label.
pub fn display(kind: &SettingKind, value: &SettingValue) -> String {
    match (kind, value) {
        (SettingKind::Int { unit: Some(u), .. }, SettingValue::Int(v)) => format!("{v} {u}"),
        (_, SettingValue::Int(v)) => v.to_string(),
        (_, SettingValue::Bool(true)) => "On".into(),
        (_, SettingValue::Bool(false)) => "Off".into(),
        (SettingKind::Enum { options, .. }, SettingValue::Str(id)) => options
            .iter()
            .find(|(o, _)| o == id)
            .map_or_else(|| id.clone(), |(_, label)| label.clone()),
        (_, SettingValue::Str(s)) => s.clone(),
        (_, other) => format!("{other:?}"),
    }
}

/// The changes "Use recommended settings" would make to `model`: each
/// setting whose recommended value ([`recommended_targets`]) differs from
/// its published one, in the tree's order; `pinned` as for
/// [`recommended_targets`].
pub fn recommended(
    model: &CameraSettings,
    row: &[(u32, i64)],
    mains: Option<u32>,
    pinned: bool,
) -> Vec<Recommended> {
    recommended_targets(model, row, mains, pinned)
        .into_iter()
        .filter_map(|(i, raw)| {
            let e = &model.entries[i];
            let target = e.values.of(raw)?;
            let current = e.setting.kind.value()?;
            (target != current).then(|| Recommended {
                setting: e.setting.id.clone(),
                label: e.setting.label.clone(),
                value: display(&e.setting.kind, &target),
                current: display(&e.setting.kind, &current),
            })
        })
        .collect()
}

/// The rate the capture mode's label shows next, given the one it shows
/// (`shown`, `None`: the mode's rate alone), the delivered rate `fps` and
/// the mode's `nominal` rate: from below [`LABEL_ON_RATIO`] of `nominal`
/// on; then back to `None` only above [`LABEL_OFF_RATIO`]; in between,
/// moved to `fps` only when it is [`LABEL_FPS_STEP`] or more away from the
/// rate shown. `None` without a nominal rate.
pub fn shown_rate(shown: Option<f64>, fps: f64, nominal: f64) -> Option<f64> {
    if !nominal.is_finite() || nominal <= 0.0 || !fps.is_finite() {
        return None;
    }
    match shown {
        None => (fps < LABEL_ON_RATIO * nominal).then_some(fps),
        Some(_) if fps > LABEL_OFF_RATIO * nominal => None,
        Some(old) if (fps - old).abs() >= LABEL_FPS_STEP => Some(fps),
        Some(old) => Some(old),
    }
}

/// Shows the delivered rate `fps` in the label of the capture mode option
/// `value` of the capture mode setting `setting`, in place of its rate part:
/// `640x480 · 30 fps` reads `640x480 · 19.5 of 30 fps`. A label without a
/// rate part (`… fps` after the last `·`) is left as it is.
pub fn mark_delivered(setting: &mut obc_ipc::Setting, value: &str, fps: f64) {
    const SEP: &str = " \u{b7} ";
    if let SettingKind::Enum { options, .. } = &mut setting.kind
        && let Some((_, label)) = options.iter_mut().find(|(v, _)| v == value)
        && let Some(rest) = label.strip_suffix(" fps")
        && let Some(i) = rest.rfind(SEP)
    {
        let (size, rate) = (&rest[..i], &rest[i + SEP.len()..]);
        *label = format!("{size}{SEP}{fps:.1} of {rate} fps");
    }
}

/// `fps` rounded to 0.1, as published.
pub fn round_fps(fps: f64) -> f64 {
    (fps * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests;
