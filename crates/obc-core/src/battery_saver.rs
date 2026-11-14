//! The battery saver (`[video] battery_saver`): under `video.memory =
//! "auto"`, a camera session runs in system memory while the saver is
//! active, so that the NVIDIA GPU is never woken for it. The default mode,
//! `auto`, activates it on battery or on power-profiles-daemon's
//! `power-saver` profile. This module holds
//! the decision from what the daemon reads of the machine (the power
//! supplies, power-profiles-daemon's active profile), and the words the
//! configuration row and the log use for it. Reading the machine is the
//! daemon's.

use serde::{Deserialize, Serialize};

use crate::config::MemoryChoice;
use crate::config_schema::DESCRIPTION_MAX;

/// `video.battery_saver`: when the battery saver is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatterySaver {
    /// Never.
    Off,
    /// While the machine runs on its battery, or power-profiles-daemon's
    /// active profile is `power-saver`; as [`BatterySaver::OnBattery`]
    /// when that service is not running. The default.
    #[default]
    Auto,
    /// While the machine runs on its battery.
    OnBattery,
    /// While power-profiles-daemon's active profile is `power-saver`; as
    /// [`BatterySaver::OnBattery`] when that service is not running.
    PowerProfile,
    /// Always.
    Always,
}

impl BatterySaver {
    /// The configuration's spelling: `off`, `auto`, `on_battery`,
    /// `power_profile`, `always`.
    pub fn as_str(self) -> &'static str {
        match self {
            BatterySaver::Off => "off",
            BatterySaver::Auto => "auto",
            BatterySaver::OnBattery => "on_battery",
            BatterySaver::PowerProfile => "power_profile",
            BatterySaver::Always => "always",
        }
    }

    /// Whether the mode reads the power supplies: `on_battery`,
    /// `power_profile` (its fallback) and `auto`. The one place that says
    /// so: [`saver_state`] ignores what a mode does not read.
    pub fn reads_power_source(self) -> bool {
        match self {
            BatterySaver::OnBattery | BatterySaver::PowerProfile | BatterySaver::Auto => true,
            BatterySaver::Off | BatterySaver::Always => false,
        }
    }

    /// Whether the mode reads power-profiles-daemon's profile:
    /// `power_profile` and `auto`. The daemon follows the service only
    /// then.
    pub fn reads_power_profile(self) -> bool {
        match self {
            BatterySaver::PowerProfile | BatterySaver::Auto => true,
            BatterySaver::Off | BatterySaver::OnBattery | BatterySaver::Always => false,
        }
    }
}

/// What the machine's power supplies show
/// (`/sys/class/power_supply`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PowerSource {
    /// A battery of the system, and no external supply online.
    Battery,
    /// An external supply (mains, USB) is online.
    External,
    /// No battery of the system: a machine without one is never on
    /// battery.
    NoBattery,
    /// The supplies could not be read (why): taken as AC power.
    Unreadable(String),
}

/// What power-profiles-daemon says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PowerProfile {
    /// Its `ActiveProfile` (`power-saver`, `balanced`, `performance`).
    Active(String),
    /// It is not running, or the system bus is unavailable.
    Unavailable,
}

/// The profile of power-profiles-daemon that activates the saver under
/// [`BatterySaver::PowerProfile`] and [`BatterySaver::Auto`].
pub const POWER_SAVER_PROFILE: &str = "power-saver";

/// Whether the battery saver is active, and why ([`saver_state`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaverState {
    /// The configuration's mode.
    pub mode: BatterySaver,
    /// The saver is active: under `video.memory = "auto"`, the next
    /// camera session runs in system memory.
    pub active: bool,
    /// Why, as a user reads it: `on battery`, `on AC power`, `no
    /// battery`, `power profile power-saver`, `always`, `turned off`;
    /// under [`BatterySaver::Auto`] while inactive, both facts, such as
    /// `on AC power, power profile balanced`.
    pub why: String,
    /// Under [`BatterySaver::PowerProfile`] or [`BatterySaver::Auto`]:
    /// power-profiles-daemon is not running, so the battery is followed
    /// instead.
    pub no_power_profiles: bool,
}

impl Default for SaverState {
    /// Inactive, under the default mode, on AC power: nothing read yet, so
    /// nothing is claimed of power-profiles-daemon.
    fn default() -> Self {
        SaverState {
            mode: BatterySaver::default(),
            active: false,
            why: battery_why(&PowerSource::External).to_string(),
            no_power_profiles: false,
        }
    }
}

/// Whether the power supplies activate the saver: on battery only.
fn battery_triggers(power: &PowerSource) -> bool {
    matches!(power, PowerSource::Battery)
}

/// What the power supplies are, as a user reads it. Supplies that could
/// not be read are taken as AC power.
fn battery_why(power: &PowerSource) -> &'static str {
    match power {
        PowerSource::Battery => "on battery",
        PowerSource::External => "on AC power",
        PowerSource::NoBattery => "no battery",
        PowerSource::Unreadable(_) => "the power supply could not be read, taken as AC power",
    }
}

/// Whether power-profiles-daemon's profile `p` activates the saver.
fn profile_triggers(p: &str) -> bool {
    p == POWER_SAVER_PROFILE
}

/// What power-profiles-daemon's profile `p` is, as a user reads it.
fn profile_why(p: &str) -> String {
    format!("power profile {p}")
}

/// The saver's state under `mode`, given the power supplies and
/// power-profiles-daemon's profile. `on_battery` follows `power`;
/// `power_profile` follows `profile` (active on [`POWER_SAVER_PROFILE`]);
/// `auto` follows both, the battery named first when both activate it;
/// `power_profile` and `auto` follow `power` alone when
/// power-profiles-daemon is unavailable; `off` and `always` are what they
/// say. Supplies that could not be read are taken as AC power.
pub fn saver_state(mode: BatterySaver, power: &PowerSource, profile: &PowerProfile) -> SaverState {
    let state = |active: bool, why: String, no_power_profiles: bool| SaverState {
        mode,
        active,
        why,
        no_power_profiles,
    };
    let by_battery =
        |missing: bool| state(battery_triggers(power), battery_why(power).into(), missing);
    match (mode, profile) {
        (BatterySaver::Off, _) => state(false, "turned off".into(), false),
        (BatterySaver::Always, _) => state(true, "always".into(), false),
        (BatterySaver::OnBattery, _) => by_battery(false),
        (BatterySaver::PowerProfile | BatterySaver::Auto, PowerProfile::Unavailable) => {
            by_battery(true)
        }
        (BatterySaver::PowerProfile, PowerProfile::Active(p)) => {
            state(profile_triggers(p), profile_why(p), false)
        }
        (BatterySaver::Auto, PowerProfile::Active(p)) => {
            if battery_triggers(power) {
                by_battery(false)
            } else if profile_triggers(p) {
                state(true, profile_why(p), false)
            } else {
                let why = format!("{}, {}", battery_why(power), profile_why(p));
                state(false, why, false)
            }
        }
    }
}

/// The fixed part of the `video.battery_saver` row's description.
pub const BATTERY_SAVER_ABOUT: &str =
    "Keeps the NVIDIA GPU asleep by processing video in system memory.";

/// The description of `video.battery_saver` when the daemon has not said
/// whether the saver is active.
pub const BATTERY_SAVER_NOTE: &str = "Keeps the NVIDIA GPU asleep by processing video in system \
     memory, while video memory is automatic. Applies at the next camera start.";

/// The note of `video.battery_saver` while a camera session runs in
/// another memory than the saver now decides.
pub const BATTERY_SAVER_NOTICE: &str = "Applies when the camera restarts";

/// Why `video.battery_saver` is not enabled under `video.memory` `cuda` or
/// `system`.
pub const BATTERY_SAVER_OFF_REASON: &str = "Set video processing to automatic to use this";

/// What power-profiles-daemon's absence makes the saver do, in its row.
pub const NO_POWER_PROFILES: &str =
    "power-profiles-daemon is not running: following the battery instead";

impl SaverState {
    /// The reason of the effective memory when the saver made it system
    /// memory: `battery saver, on battery`, `battery saver, power profile
    /// power-saver`, `battery saver, always`.
    pub fn memory_reason(&self) -> String {
        format!("battery saver, {}", self.why)
    }

    /// The text of the `video.battery_saver` row under `choice` (the
    /// saver acts only under `auto`): [`BATTERY_SAVER_ABOUT`], then `Now:`
    /// and whether it is active and why, such as `Now: active, on
    /// battery.`; with power-profiles-daemon missing, `Now:
    /// power-profiles-daemon is not running: following the battery
    /// instead; inactive, on AC power.`, without the fixed part (both
    /// would not fit [`DESCRIPTION_MAX`] bytes). Shortened beyond.
    pub fn description(&self, choice: MemoryChoice) -> String {
        let now = if choice != MemoryChoice::Auto {
            "Now: inactive, video processing is not automatic.".to_string()
        } else {
            let state = if self.active { "active" } else { "inactive" };
            let missing = if self.no_power_profiles {
                format!("{NO_POWER_PROFILES}; ")
            } else {
                String::new()
            };
            format!("Now: {missing}{state}, {}.", self.why)
        };
        let whole = format!("{BATTERY_SAVER_ABOUT} {now}");
        if whole.len() <= DESCRIPTION_MAX
            && !(self.no_power_profiles && choice == MemoryChoice::Auto)
        {
            return whole;
        }
        let mut now = now;
        if now.len() > DESCRIPTION_MAX {
            let mut end = DESCRIPTION_MAX - '…'.len_utf8();
            while !now.is_char_boundary(end) {
                end -= 1;
            }
            now.truncate(end);
            now.push('…');
        }
        now
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [PowerSource; 4] = [
        PowerSource::Battery,
        PowerSource::External,
        PowerSource::NoBattery,
        PowerSource::Unreadable(String::new()),
    ];

    #[test]
    fn the_modes_parse_as_spelled() {
        for mode in [
            BatterySaver::Off,
            BatterySaver::Auto,
            BatterySaver::OnBattery,
            BatterySaver::PowerProfile,
            BatterySaver::Always,
        ] {
            let text = format!("m = \"{}\"", mode.as_str());
            #[derive(Deserialize)]
            struct M {
                m: BatterySaver,
            }
            assert_eq!(toml::from_str::<M>(&text).unwrap().m, mode);
        }
        assert_eq!(BatterySaver::Auto.as_str(), "auto");
        assert_eq!(BatterySaver::default(), BatterySaver::Auto);
    }

    #[test]
    fn each_mode_reads_what_it_follows() {
        let modes = [
            (BatterySaver::Off, false, false),
            (BatterySaver::Auto, true, true),
            (BatterySaver::OnBattery, true, false),
            (BatterySaver::PowerProfile, true, true),
            (BatterySaver::Always, false, false),
        ];
        for (mode, power, profile) in modes {
            assert_eq!(mode.reads_power_source(), power, "{mode:?}");
            assert_eq!(mode.reads_power_profile(), profile, "{mode:?}");
        }
        // What a mode does not read changes nothing of its state.
        let profiles = [
            PowerProfile::Unavailable,
            PowerProfile::Active("power-saver".into()),
            PowerProfile::Active("balanced".into()),
        ];
        for (mode, power, profile) in modes {
            for p in &ALL {
                for pr in &profiles {
                    let read_p = if power { p } else { &PowerSource::External };
                    let read_pr = if profile {
                        pr
                    } else {
                        &PowerProfile::Unavailable
                    };
                    assert_eq!(
                        saver_state(mode, p, pr),
                        saver_state(mode, read_p, read_pr),
                        "{mode:?} {p:?} {pr:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_default_state_is_inactive_on_ac_and_claims_nothing_of_the_profile() {
        let d = SaverState::default();
        assert_eq!(d.mode, BatterySaver::Auto);
        assert!(!d.active);
        assert_eq!(d.why, "on AC power");
        assert!(!d.no_power_profiles);
    }

    #[test]
    fn auto_follows_the_battery_or_the_power_saver_profile() {
        let saver = PowerProfile::Active("power-saver".into());
        let balanced = PowerProfile::Active("balanced".into());
        let unreadable = "the power supply could not be read, taken as AC power";
        let cases: [(PowerSource, &PowerProfile, bool, String); 8] = [
            // Battery wins the wording when both hold.
            (PowerSource::Battery, &saver, true, "on battery".into()),
            (PowerSource::Battery, &balanced, true, "on battery".into()),
            (
                PowerSource::External,
                &saver,
                true,
                "power profile power-saver".into(),
            ),
            (
                PowerSource::External,
                &balanced,
                false,
                "on AC power, power profile balanced".into(),
            ),
            (
                PowerSource::NoBattery,
                &saver,
                true,
                "power profile power-saver".into(),
            ),
            (
                PowerSource::NoBattery,
                &balanced,
                false,
                "no battery, power profile balanced".into(),
            ),
            (
                PowerSource::Unreadable("EIO".into()),
                &saver,
                true,
                "power profile power-saver".into(),
            ),
            (
                PowerSource::Unreadable("EIO".into()),
                &balanced,
                false,
                format!("{unreadable}, power profile balanced"),
            ),
        ];
        for (power, profile, active, why) in cases {
            let s = saver_state(BatterySaver::Auto, &power, profile);
            assert_eq!((s.active, &s.why), (active, &why), "{power:?} {profile:?}");
            assert!(!s.no_power_profiles, "{power:?} {profile:?}");
            assert_eq!(s.mode, BatterySaver::Auto);
        }
        // Without the service: as `on_battery`, and it says so.
        for p in &ALL {
            let s = saver_state(BatterySaver::Auto, p, &PowerProfile::Unavailable);
            let b = saver_state(BatterySaver::OnBattery, p, &PowerProfile::Unavailable);
            assert_eq!((s.active, &s.why), (b.active, &b.why));
            assert!(s.no_power_profiles);
        }
        assert_eq!(
            saver_state(BatterySaver::Auto, &PowerSource::External, &saver).memory_reason(),
            "battery saver, power profile power-saver"
        );
    }

    #[test]
    fn auto_rows_keep_the_fixed_part_in_the_common_cases() {
        let row = |power: PowerSource, profile: &str| {
            saver_state(
                BatterySaver::Auto,
                &power,
                &PowerProfile::Active(profile.into()),
            )
            .description(MemoryChoice::Auto)
        };
        for power in [
            PowerSource::Battery,
            PowerSource::External,
            PowerSource::NoBattery,
        ] {
            for profile in ["power-saver", "balanced", "performance"] {
                let d = row(power.clone(), profile);
                assert!(d.starts_with(BATTERY_SAVER_ABOUT), "{d}");
                assert!(d.len() <= DESCRIPTION_MAX, "{} bytes: {d}", d.len());
            }
        }
        assert_eq!(
            row(PowerSource::External, "balanced"),
            "Keeps the NVIDIA GPU asleep by processing video in system memory. \
             Now: inactive, on AC power, power profile balanced."
        );
        // Too long with the fixed part: shortened, still said.
        let d = row(PowerSource::Unreadable("EIO".into()), "performance");
        assert!(d.len() <= DESCRIPTION_MAX, "{} bytes: {d}", d.len());
        assert!(d.contains("power profile performance"), "{d}");
    }

    #[test]
    fn on_battery_follows_the_supplies_and_takes_unreadable_as_ac() {
        let s =
            |p: &PowerSource| saver_state(BatterySaver::OnBattery, p, &PowerProfile::Unavailable);
        let got: Vec<(bool, String)> = ALL.iter().map(|p| (s(p).active, s(p).why)).collect();
        assert_eq!(
            got,
            [
                (true, "on battery".to_string()),
                (false, "on AC power".to_string()),
                (false, "no battery".to_string()),
                (
                    false,
                    "the power supply could not be read, taken as AC power".to_string()
                ),
            ]
        );
        assert!(ALL.iter().all(|p| !s(p).no_power_profiles));
    }

    #[test]
    fn off_and_always_ignore_the_machine() {
        for p in &ALL {
            for profile in [
                PowerProfile::Unavailable,
                PowerProfile::Active("power-saver".into()),
            ] {
                let off = saver_state(BatterySaver::Off, p, &profile);
                assert_eq!((off.active, off.why.as_str()), (false, "turned off"));
                let on = saver_state(BatterySaver::Always, p, &profile);
                assert_eq!((on.active, on.why.as_str()), (true, "always"));
            }
        }
    }

    #[test]
    fn power_profile_follows_the_profile_else_the_battery() {
        for p in &ALL {
            let saver = saver_state(
                BatterySaver::PowerProfile,
                p,
                &PowerProfile::Active("power-saver".into()),
            );
            assert!(saver.active);
            assert_eq!(saver.why, "power profile power-saver");
            assert_eq!(
                saver.memory_reason(),
                "battery saver, power profile power-saver"
            );
            for other in ["balanced", "performance"] {
                let s = saver_state(
                    BatterySaver::PowerProfile,
                    p,
                    &PowerProfile::Active(other.into()),
                );
                assert!(!s.active, "{other}");
                assert_eq!(s.why, format!("power profile {other}"));
                assert!(!s.no_power_profiles);
            }
        }
        // Without the service: as `on_battery`, and it says so.
        for p in &ALL {
            let s = saver_state(BatterySaver::PowerProfile, p, &PowerProfile::Unavailable);
            let b = saver_state(BatterySaver::OnBattery, p, &PowerProfile::Unavailable);
            assert_eq!((s.active, &s.why), (b.active, &b.why));
            assert!(s.no_power_profiles);
        }
    }

    #[test]
    fn the_row_says_whether_it_is_active_and_why() {
        let on = |p: PowerSource, profile: PowerProfile, mode: BatterySaver| {
            saver_state(mode, &p, &profile).description(MemoryChoice::Auto)
        };
        assert_eq!(
            on(
                PowerSource::Battery,
                PowerProfile::Unavailable,
                BatterySaver::OnBattery
            ),
            "Keeps the NVIDIA GPU asleep by processing video in system memory. \
             Now: active, on battery."
        );
        assert_eq!(
            on(
                PowerSource::External,
                PowerProfile::Unavailable,
                BatterySaver::OnBattery
            ),
            "Keeps the NVIDIA GPU asleep by processing video in system memory. \
             Now: inactive, on AC power."
        );
        assert_eq!(
            on(
                PowerSource::External,
                PowerProfile::Active("power-saver".into()),
                BatterySaver::PowerProfile
            ),
            "Keeps the NVIDIA GPU asleep by processing video in system memory. \
             Now: active, power profile power-saver."
        );
        assert_eq!(
            on(
                PowerSource::NoBattery,
                PowerProfile::Unavailable,
                BatterySaver::Always
            ),
            "Keeps the NVIDIA GPU asleep by processing video in system memory. \
             Now: active, always."
        );
        // The service missing: said, the fixed part dropped to fit.
        let d = on(
            PowerSource::External,
            PowerProfile::Unavailable,
            BatterySaver::PowerProfile,
        );
        assert_eq!(
            d,
            "Now: power-profiles-daemon is not running: following the battery instead; \
             inactive, on AC power."
        );
        let d = on(
            PowerSource::Unreadable("EIO".into()),
            PowerProfile::Unavailable,
            BatterySaver::PowerProfile,
        );
        assert!(d.len() <= DESCRIPTION_MAX, "{} bytes: {d}", d.len());
        assert!(
            d.starts_with("Now: power-profiles-daemon is not running"),
            "{d}"
        );
        // Not under `auto`: the saver does nothing.
        let s = saver_state(
            BatterySaver::Always,
            &PowerSource::Battery,
            &PowerProfile::Unavailable,
        );
        for choice in [MemoryChoice::Cuda, MemoryChoice::System] {
            assert_eq!(
                s.description(choice),
                "Keeps the NVIDIA GPU asleep by processing video in system memory. \
                 Now: inactive, video processing is not automatic."
            );
        }
        for d in [BATTERY_SAVER_NOTE, BATTERY_SAVER_ABOUT] {
            assert!(d.len() <= DESCRIPTION_MAX);
        }
    }
}
