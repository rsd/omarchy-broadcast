//! What the battery saver reads of the machine (`[video] battery_saver`,
//! [`obc_core::battery_saver`]): the power supplies in sysfs, read when
//! asked (cheap, no device is woken), and power-profiles-daemon's active
//! profile on the system bus, followed by a task of the Tokio runtime
//! ([`run_profile_watch`]) into a cache ([`ProfileCache`]) that the
//! controller reads. The controller thread never waits on D-Bus.
//!
//! The task follows the profile only while the controller asks for it
//! ([`ProfileCache::follow`]: the mode is `power_profile` or `auto`);
//! otherwise it holds no system bus connection. It never starts
//! power-profiles-daemon: the service is D-Bus activatable, so a call to
//! its name would start it (on a machine that manages power otherwise,
//! such as with TLP). Its
//! property is read only while its name has an owner, and with
//! `NO_AUTO_START`; the service starting later is learnt from
//! `NameOwnerChanged`. A failure of the bus is retried, every
//! [`RETRY_FIRST`] doubling up to [`RETRY_MAX`], while followed.

use std::fs;
use std::future::Future;
use std::io;
use std::path::{Component, Path};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures_util::StreamExt;
use obc_core::battery_saver::{PowerProfile, PowerSource};
use tokio::sync::watch;
use zbus::Connection;
use zbus::OwnedVariant;
use zbus::names::BusName;
use zbus::proxy::MethodFlags;

use crate::{debug, info};

/// Where the kernel lists the power supplies.
pub const POWER_SUPPLY_ROOT: &str = "/sys/class/power_supply";

/// The power supplies under `root` (a `/sys/class/power_supply` tree): on
/// battery when at least one battery of the machine is listed
/// (`machine_battery`: a mouse's or a headset's battery is not the
/// machine's) and no external supply (`type` `Mains`, `Wireless`, or `USB`
/// and its kinds: `USB_C`, `USB_PD`...) reports `online` 1. No battery of
/// the machine: never on battery. A missing `root` lists no supply. A
/// supply that cannot be read is left out; only when none can be read
/// (and one is listed), or `root` itself cannot be listed, are the
/// supplies unreadable (taken as AC power by the saver).
pub fn power_source(root: &Path) -> PowerSource {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return PowerSource::NoBattery,
        Err(e) => return PowerSource::Unreadable(format!("{}: {e}", root.display())),
    };
    let mut battery = false;
    let mut external_online = false;
    let mut read = false;
    let mut failure: Option<String> = None;
    for entry in entries {
        let supply = entry
            .map_err(|e| format!("{}: {e}", root.display()))
            .and_then(|entry| read_supply(&entry.path()));
        match supply {
            Ok(kind) => {
                read = true;
                match kind {
                    Supply::MachineBattery => battery = true,
                    Supply::External { online } => external_online |= online,
                    Supply::Other => {}
                }
            }
            Err(e) => {
                failure.get_or_insert(e);
            }
        }
    }
    if !read && let Some(why) = failure {
        return PowerSource::Unreadable(why);
    }
    match (battery, external_online) {
        (false, _) => PowerSource::NoBattery,
        (true, true) => PowerSource::External,
        (true, false) => PowerSource::Battery,
    }
}

/// What a supply counts as, for [`power_source`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Supply {
    /// A battery of the machine ([`machine_battery`]).
    MachineBattery,
    /// An external supply (mains, wireless charger, USB), online or not.
    External { online: bool },
    /// Anything else: a peripheral's battery, a supply of another kind.
    Other,
}

/// Reads the supply at `dir` (an entry of the class); why it cannot be.
fn read_supply(dir: &Path) -> Result<Supply, String> {
    let attr = |name: &str| -> Result<Option<String>, String> {
        match fs::read_to_string(dir.join(name)) {
            Ok(s) => Ok(Some(s.trim().to_string())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", dir.join(name).display())),
        }
    };
    let Some(kind) = attr("type")? else {
        return Ok(Supply::Other);
    };
    Ok(match kind.as_str() {
        "Battery" if machine_battery(dir, attr("scope")?.as_deref()) => Supply::MachineBattery,
        "Mains" | "Wireless" => Supply::External {
            online: attr("online")?.as_deref() == Some("1"),
        },
        k if k.starts_with("USB") => Supply::External {
            online: attr("online")?.as_deref() == Some("1"),
        },
        _ => Supply::Other,
    })
}

/// Whether the battery at `dir` (a supply of `type` `Battery`, with its
/// `scope`) is the machine's: `scope` `System` says so and `Device` says it
/// is a peripheral's. Without a scope (or `Unknown`), it is the machine's
/// when its device is an ACPI battery (under `LNXSYSTM`, or a `PNP0C0A`
/// device) or, failing that, when it is named `BAT…`; a battery whose
/// device is on USB or HID (a wireless mouse's, a headset's, a gamepad's)
/// never is.
fn machine_battery(dir: &Path, scope: Option<&str>) -> bool {
    match scope {
        Some("System") => return true,
        Some("Device") => return false,
        _ => {}
    }
    // The device the supply belongs to; the supply's own place in sysfs
    // otherwise (it lies under its device).
    let device = fs::canonicalize(dir.join("device"))
        .or_else(|_| fs::canonicalize(dir))
        .ok();
    let names: Vec<String> = device
        .iter()
        .flat_map(|d| d.components())
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    if names
        .iter()
        .any(|n| n.starts_with("usb") || n.to_ascii_lowercase().contains("hid") || hid_id(n))
    {
        return false;
    }
    if names
        .iter()
        .any(|n| n.starts_with("LNXSYSTM") || n.contains("PNP0C0A"))
    {
        return true;
    }
    dir.file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with("BAT"))
}

/// Whether `name` is a HID device's in sysfs: `bus:vendor:product.n`, four
/// hexadecimal digits each (`0005:046D:B023.0007`; a PCI address,
/// `0000:00:14.0`, is not one).
fn hid_id(name: &str) -> bool {
    let hex = |s: &str| s.len() == 4 && s.bytes().all(|b| b.is_ascii_hexdigit());
    let mut parts = name.split(':');
    let (Some(bus), Some(vendor), Some(rest), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let Some((product, n)) = rest.split_once('.') else {
        return false;
    };
    [bus, vendor, product, n].into_iter().all(hex)
}

/// power-profiles-daemon's names, objects and interfaces, the current one
/// first: `(bus name, object path, interface)`.
pub const POWER_PROFILES: [(&str, &str, &str); 2] = [
    (
        "org.freedesktop.UPower.PowerProfiles",
        "/org/freedesktop/UPower/PowerProfiles",
        "org.freedesktop.UPower.PowerProfiles",
    ),
    (
        "net.hadess.PowerProfiles",
        "/net/hadess/PowerProfiles",
        "net.hadess.PowerProfiles",
    ),
];

/// The property that names the active profile.
const ACTIVE_PROFILE: &str = "ActiveProfile";

/// The first wait before following power-profiles-daemon again after a
/// failure (of the system bus, of a subscription); doubled at each failure
/// in a row, up to [`RETRY_MAX`].
pub const RETRY_FIRST: Duration = Duration::from_secs(2);
/// The longest wait between two attempts.
pub const RETRY_MAX: Duration = Duration::from_secs(60);

/// power-profiles-daemon's profile as last read: written by
/// [`run_profile_watch`], read by the controller (through the backend)
/// without waiting. [`PowerProfile::Unavailable`] until the first read,
/// and while it is not followed. With it, whether the controller wants it
/// followed ([`Self::follow`]), which starts and stops the watcher; and
/// whether the watcher has read it since ([`Self::follow_and_read`]).
#[derive(Debug, Clone)]
pub struct ProfileCache {
    profile: Arc<Mutex<PowerProfile>>,
    wanted: watch::Sender<bool>,
    /// Whether the watcher has read the profile (or failed to reach the
    /// bus) since it was last asked to follow it.
    read: watch::Sender<bool>,
}

impl Default for ProfileCache {
    fn default() -> Self {
        ProfileCache {
            profile: Arc::new(Mutex::new(PowerProfile::Unavailable)),
            wanted: watch::channel(false).0,
            read: watch::channel(false).0,
        }
    }
}

impl ProfileCache {
    /// The profile as last read.
    pub fn get(&self) -> PowerProfile {
        self.profile
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Records `profile`; whether it changed.
    pub fn set(&self, profile: PowerProfile) -> bool {
        let mut p = self.profile.lock().unwrap_or_else(PoisonError::into_inner);
        if *p == profile {
            return false;
        }
        *p = profile;
        true
    }

    /// Whether the profile is to be followed (the battery saver's mode is
    /// `power_profile` or `auto`): the watcher follows it from now on, or
    /// stops and lets go of the system bus. Cheap when nothing changes.
    pub fn follow(&self, on: bool) {
        let started = self.wanted.send_if_modified(|w| {
            let changed = *w != on;
            *w = on;
            changed
        }) && on;
        if started {
            self.read.send_replace(false);
        }
    }

    /// Follows the profile ([`Self::follow`]) and waits, up to `bound`,
    /// for the watcher's first read of it: the profile is then
    /// [`Self::get`]'s, or [`PowerProfile::Unavailable`] for a service
    /// that is not running or a bus that could not be reached. Whether
    /// the read came in time; past `bound` the cache is left as it is
    /// (unavailable until the read) and the watcher carries on. Returns at
    /// once when the profile was already read since it is followed. For
    /// the daemon's start, so the first decision of the battery saver is
    /// not taken before the profile is known.
    pub async fn follow_and_read(&self, bound: Duration) -> bool {
        self.follow(true);
        let mut read = self.read.subscribe();
        matches!(
            tokio::time::timeout(bound, read.wait_for(|r| *r)).await,
            Ok(Ok(_))
        )
    }

    /// Notes that the watcher read the profile, or found it could not.
    fn note_read(&self) {
        self.read.send_replace(true);
    }

    /// Whether the profile is to be followed now.
    pub fn followed(&self) -> bool {
        *self.wanted.borrow()
    }
}

/// The active profile of the first of [`POWER_PROFILES`] whose name has an
/// owner on `conn`; [`PowerProfile::Unavailable`] when none has. With it,
/// the index of the service that answered. The property is read with
/// `NO_AUTO_START`: a name whose owner left after the check is not
/// activated. An error of the bus itself is returned.
async fn read_profile(
    conn: &Connection,
    dbus: &zbus::fdo::DBusProxy<'_>,
) -> zbus::Result<(PowerProfile, Option<usize>)> {
    for (i, (name, path, iface)) in POWER_PROFILES.iter().enumerate() {
        if !dbus.name_has_owner(BusName::try_from(*name)?).await? {
            continue;
        }
        let Ok(proxy) = properties(conn, name, path).await else {
            continue;
        };
        let reply = proxy
            .inner()
            .call_with_flags::<_, _, OwnedVariant>(
                "Get",
                MethodFlags::NO_AUTO_START,
                &(*iface, ACTIVE_PROFILE),
            )
            .await;
        if let Ok(Some(v)) = reply
            && let Ok(p) = String::try_from(&*v)
        {
            return Ok((PowerProfile::Active(p), Some(i)));
        }
    }
    Ok((PowerProfile::Unavailable, None))
}

async fn properties<'a>(
    conn: &Connection,
    name: &'a str,
    path: &'a str,
) -> zbus::Result<zbus::fdo::PropertiesProxy<'a>> {
    zbus::fdo::PropertiesProxy::builder(conn)
        .destination(name)
        .path(path)
        // Nothing to cache (a fetch would also activate the service): the
        // property is read and followed by hand.
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
}

/// Logs a change of power-profiles-daemon's availability, once per change.
fn note_availability(was: Option<bool>, now: bool) {
    if was == Some(now) {
        return;
    }
    if now {
        info!("battery saver: power-profiles-daemon is running");
    } else if was.is_some() {
        info!("battery saver: power-profiles-daemon stopped: following the battery instead");
    } else {
        info!("battery saver: power-profiles-daemon is not running: following the battery instead");
    }
}

/// Follows power-profiles-daemon's active profile on the system bus into
/// `cache` while the cache asks for it ([`ProfileCache::follow`]), calling
/// `changed` after each change: read at once, then again on each
/// `PropertiesChanged` of its object and each change of owner of its names
/// (the service starting or stopping); never starting the service (see the
/// module docs). Not followed, it holds no connection and the cache is
/// [`PowerProfile::Unavailable`]. A failure (no system bus, a lost
/// connection) makes the cache unavailable and is retried after
/// [`RETRY_FIRST`], doubling up to [`RETRY_MAX`]. One info line is logged
/// at each change of the service's availability, and at the first failure
/// in a row. Ends once the cache is dropped.
pub async fn run_profile_watch(cache: ProfileCache, changed: impl Fn() + Send + 'static) {
    watch_profiles(cache, changed, Connection::system, RETRY_FIRST).await;
}

/// [`run_profile_watch`] on the bus `connect` opens, retrying after
/// `first_retry` (doubled up to [`RETRY_MAX`]).
async fn watch_profiles<F, Fut>(
    cache: ProfileCache,
    changed: impl Fn() + Send,
    connect: F,
    first_retry: Duration,
) where
    F: Fn() -> Fut,
    Fut: Future<Output = zbus::Result<Connection>>,
{
    let mut wanted = cache.wanted.subscribe();
    let mut retry = first_retry;
    let mut failing = false;
    loop {
        // Idle, with no connection, until a mode reads the profile.
        if wanted.wait_for(|w| *w).await.is_err() {
            return;
        }
        let mut available = None;
        let mut reached = false;
        let outcome = tokio::select! {
            e = follow_profiles(&connect, &cache, &changed, &mut available, &mut reached) => Some(e),
            stop = wanted.wait_for(|w| !*w) => {
                drop(stop);
                None
            }
        };
        let Some(e) = outcome else {
            // No longer followed: what was read must not be taken for the
            // profile once it is followed again.
            debug!("battery saver: power-profiles-daemon is no longer followed");
            if cache.set(PowerProfile::Unavailable) {
                changed();
            }
            retry = first_retry;
            failing = false;
            continue;
        };
        if reached {
            // It was followed: a new failure, not one more in a row.
            retry = first_retry;
            failing = false;
        }
        if failing {
            debug!(
                "battery saver: following power-profiles-daemon failed again ({e}): retrying in \
                 {}s",
                retry.as_secs_f32()
            );
        } else {
            info!(
                "battery saver: following power-profiles-daemon failed ({e}): the battery is \
                 followed, retrying in {}s",
                retry.as_secs_f32()
            );
        }
        failing = true;
        if cache.set(PowerProfile::Unavailable) {
            changed();
        }
        // Unavailable is the answer until the retry.
        cache.note_read();
        tokio::select! {
            () = tokio::time::sleep(retry) => {}
            stop = wanted.wait_for(|w| !*w) => drop(stop),
        }
        retry = (retry * 2).min(RETRY_MAX);
    }
}

/// One attempt of [`watch_profiles`]: connects, then follows until the
/// bus fails (its error). `reached` is set once the profile was read.
async fn follow_profiles<F, Fut>(
    connect: &F,
    cache: &ProfileCache,
    changed: &(impl Fn() + Send),
    available: &mut Option<bool>,
    reached: &mut bool,
) -> zbus::Error
where
    F: Fn() -> Fut,
    Fut: Future<Output = zbus::Result<Connection>>,
{
    let conn = match connect().await {
        Ok(c) => c,
        Err(e) => return e,
    };
    match follow_on(&conn, cache, changed, available, reached).await {
        Ok(never) => match never {},
        Err(e) => e,
    }
}

async fn follow_on(
    conn: &Connection,
    cache: &ProfileCache,
    changed: &(impl Fn() + Send),
    available: &mut Option<bool>,
    reached: &mut bool,
) -> zbus::Result<std::convert::Infallible> {
    let dbus = zbus::fdo::DBusProxy::builder(conn)
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?;
    // Only the changes of owner of its names wake the task: subscribed
    // before the first read, so a start in between is not missed.
    let [(current, ..), (legacy, ..)] = POWER_PROFILES;
    let mut owners = futures_util::stream::select(
        dbus.receive_name_owner_changed_with_args(&[(0, current)])
            .await?,
        dbus.receive_name_owner_changed_with_args(&[(0, legacy)])
            .await?,
    );
    loop {
        let (profile, service) = read_profile(conn, &dbus).await?;
        *reached = true;
        note_availability(*available, service.is_some());
        *available = Some(service.is_some());
        if cache.set(profile) {
            changed();
        }
        cache.note_read();
        // Its properties are followed while it runs.
        let mut props = match service {
            Some(i) => {
                let (name, path, _) = POWER_PROFILES[i];
                Some(
                    properties(conn, name, path)
                        .await?
                        .receive_properties_changed()
                        .await?,
                )
            }
            None => None,
        };
        // A change between the read and the subscription is not missed.
        if props.is_some() && cache.set(read_profile(conn, &dbus).await?.0) {
            changed();
        }
        loop {
            tokio::select! {
                signal = owners.next() => {
                    if signal.is_none() {
                        return Err(zbus::Error::Failure("the system bus closed".into()));
                    }
                    break;
                }
                signal = async {
                    match props.as_mut() {
                        Some(p) => p.next().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let Some(signal) = signal else { break };
                    let Ok(args) = signal.args() else { continue };
                    if args.changed_properties().contains_key(ACTIVE_PROFILE)
                        || args.invalidated_properties().contains(&ACTIVE_PROFILE)
                    {
                        break;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Instant;

    use super::*;
    use crate::dbus::tests::PrivateBus;

    /// Writes a supply `name` with its attributes under `root`.
    fn supply(root: &Path, name: &str, attrs: &[(&str, &str)]) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        for (k, v) in attrs {
            fs::write(dir.join(k), format!("{v}\n")).unwrap();
        }
    }

    /// Gives the supply `name` under `root` a `device` link to a device at
    /// `device` (a path under `root/devices`, made).
    fn device(root: &Path, name: &str, device: &str) {
        let target = root.join("devices").join(device);
        fs::create_dir_all(&target).unwrap();
        symlink(&target, root.join(name).join("device")).unwrap();
    }

    /// An ACPI battery's device, as a laptop's sysfs shows it.
    const ACPI_BATTERY: &str = "LNXSYSTM:00/LNXSYBUS:00/PNP0A08:00/device:00/PNP0C09:00/PNP0C0A:00";
    /// A wireless mouse's battery device (HID over a USB receiver).
    const HID_BATTERY: &str =
        "pci0000:00/0000:00:14.0/usb1/1-2/1-2:1.2/0003:046D:C52B.0003/0003:046D:4082.0004";
    /// A Bluetooth headset's battery device (HID over Bluetooth).
    const BT_BATTERY: &str = "pci0000:00/0000:00:14.0/bluetooth/hci0/hci0:256/0005:054C:0CE6.0007";

    fn laptop(ac_online: &str) -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        supply(
            t.path(),
            "BAT0",
            &[("type", "Battery"), ("scope", "System")],
        );
        supply(t.path(), "AC", &[("type", "Mains"), ("online", ac_online)]);
        t
    }

    #[test]
    fn a_laptop_on_ac_is_on_external_power() {
        let t = laptop("1");
        assert_eq!(power_source(t.path()), PowerSource::External);
    }

    #[test]
    fn a_laptop_with_ac_offline_is_on_battery() {
        let t = laptop("0");
        assert_eq!(power_source(t.path()), PowerSource::Battery);
        // A battery alone (no AC entry, no scope), named as ACPI names it:
        // on battery too.
        let t = tempfile::tempdir().unwrap();
        supply(t.path(), "BAT1", &[("type", "Battery")]);
        assert_eq!(power_source(t.path()), PowerSource::Battery);
    }

    #[test]
    fn a_battery_without_scope_counts_only_when_it_is_the_machines() {
        // Its device an ACPI battery, whatever its name.
        let t = tempfile::tempdir().unwrap();
        supply(t.path(), "CMB0", &[("type", "Battery")]);
        device(t.path(), "CMB0", ACPI_BATTERY);
        assert_eq!(power_source(t.path()), PowerSource::Battery);
        // A peripheral's, on HID over USB or Bluetooth, on a desktop: no
        // battery of the machine, even named `BAT…`.
        for (name, dev) in [
            ("hidpp_battery_0", HID_BATTERY),
            ("ps-controller-battery-a0:ab:51", BT_BATTERY),
            ("BAT9", HID_BATTERY),
        ] {
            let t = tempfile::tempdir().unwrap();
            supply(t.path(), name, &[("type", "Battery")]);
            device(t.path(), name, dev);
            assert_eq!(power_source(t.path()), PowerSource::NoBattery, "{name}");
        }
        // Neither scope, device nor name says it: not counted.
        let t = tempfile::tempdir().unwrap();
        supply(t.path(), "some_battery", &[("type", "Battery")]);
        assert_eq!(power_source(t.path()), PowerSource::NoBattery);
        // `Unknown` is no scope.
        let t = tempfile::tempdir().unwrap();
        supply(
            t.path(),
            "BAT0",
            &[("type", "Battery"), ("scope", "Unknown")],
        );
        assert_eq!(power_source(t.path()), PowerSource::Battery);
        // A peripheral next to the machine's battery changes nothing.
        let t = laptop("1");
        supply(t.path(), "hidpp_battery_0", &[("type", "Battery")]);
        device(t.path(), "hidpp_battery_0", HID_BATTERY);
        assert_eq!(power_source(t.path()), PowerSource::External);
    }

    #[test]
    fn hid_devices_are_recognised_by_name() {
        assert!(hid_id("0003:046D:4082.0004"));
        assert!(hid_id("0005:054C:0CE6.0007"));
        for name in [
            "0000:00:14.0",
            "PNP0C0A:00",
            "1-2:1.2",
            "0003:046D:4082",
            "a:b:c.g",
        ] {
            assert!(!hid_id(name), "{name}");
        }
    }

    #[test]
    fn a_desktop_without_a_battery_is_never_on_battery() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(power_source(t.path()), PowerSource::NoBattery);
        // A wireless mouse's battery is no battery of the machine.
        supply(
            t.path(),
            "hidpp_battery_0",
            &[("type", "Battery"), ("scope", "Device")],
        );
        supply(
            t.path(),
            "ucsi-source-psy-1",
            &[("type", "USB"), ("online", "0")],
        );
        assert_eq!(power_source(t.path()), PowerSource::NoBattery);
        // No supply class at all.
        assert_eq!(
            power_source(&t.path().join("missing")),
            PowerSource::NoBattery
        );
    }

    #[test]
    fn a_usb_c_or_wireless_supply_online_is_external_power() {
        for kind in ["USB", "USB_C", "USB_PD", "USB_PD_DRP", "Wireless"] {
            let t = laptop("0");
            supply(
                t.path(),
                "ucsi-source-psy-USBC000:001",
                &[("type", kind), ("online", "1")],
            );
            assert_eq!(power_source(t.path()), PowerSource::External, "{kind}");
        }
        // Offline, it changes nothing.
        let t = laptop("0");
        supply(t.path(), "usb-c", &[("type", "USB_C"), ("online", "0")]);
        supply(t.path(), "qi", &[("type", "Wireless"), ("online", "0")]);
        assert_eq!(power_source(t.path()), PowerSource::Battery);
    }

    #[test]
    fn a_supply_that_cannot_be_read_is_left_out() {
        // `online` is a directory: reading it fails. The battery is read.
        let t = laptop("0");
        supply(t.path(), "USBC", &[("type", "USB_C")]);
        fs::create_dir(t.path().join("USBC/online")).unwrap();
        assert_eq!(power_source(t.path()), PowerSource::Battery);
        // An unreadable battery next to readable AC: no battery read.
        let t = laptop("1");
        fs::remove_file(t.path().join("BAT0/scope")).unwrap();
        fs::create_dir(t.path().join("BAT0/scope")).unwrap();
        assert_eq!(power_source(t.path()), PowerSource::NoBattery);
    }

    #[test]
    fn supplies_none_of_which_can_be_read_are_unreadable() {
        let t = tempfile::tempdir().unwrap();
        supply(t.path(), "AC", &[("type", "Mains")]);
        fs::create_dir(t.path().join("AC/online")).unwrap();
        let PowerSource::Unreadable(why) = power_source(t.path()) else {
            panic!("readable");
        };
        assert!(why.contains("AC/online"), "{why}");
        // The class itself is a file.
        let t = tempfile::tempdir().unwrap();
        let file = t.path().join("power_supply");
        fs::write(&file, "").unwrap();
        assert!(matches!(power_source(&file), PowerSource::Unreadable(_)));
    }

    /// On a machine with a system bus: the watcher reads the profile when
    /// power-profiles-daemon runs (`cargo test -- --ignored`).
    #[test]
    #[ignore = "needs the system bus"]
    fn the_watcher_reads_the_profile_off_the_system_bus() {
        let rt = runtime();
        rt.block_on(async {
            let conn = Connection::system().await.unwrap();
            let dbus = zbus::fdo::DBusProxy::new(&conn).await.unwrap();
            let (profile, service) = read_profile(&conn, &dbus).await.unwrap();
            eprintln!("{profile:?} from {service:?}");
            let cache = ProfileCache::default();
            cache.follow(true);
            let task = tokio::spawn(run_profile_watch(cache.clone(), || {}));
            for _ in 0..50 {
                if cache.get() == profile {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert_eq!(cache.get(), profile);
            task.abort();
        });
        eprintln!("{:?}", power_source(Path::new(POWER_SUPPLY_ROOT)));
    }

    #[test]
    fn the_cache_reports_changes_only() {
        let c = ProfileCache::default();
        assert_eq!(c.get(), PowerProfile::Unavailable);
        assert!(!c.set(PowerProfile::Unavailable));
        assert!(c.set(PowerProfile::Active("power-saver".into())));
        assert!(!c.clone().set(PowerProfile::Active("power-saver".into())));
        assert_eq!(c.get(), PowerProfile::Active("power-saver".into()));
        assert!(!c.followed());
        c.follow(true);
        assert!(c.clone().followed());
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// A stand-in for power-profiles-daemon's object.
    struct FakeProfiles(String);

    #[zbus::interface(name = "org.freedesktop.UPower.PowerProfiles")]
    impl FakeProfiles {
        #[zbus(property)]
        fn active_profile(&self) -> String {
            self.0.clone()
        }
    }

    /// Waits (up to 5 s) until `cache` holds `want`.
    async fn until(cache: &ProfileCache, want: &PowerProfile) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while cache.get() != *want {
            assert!(Instant::now() < deadline, "{:?}, not {want:?}", cache.get());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A private bus on which both power-profiles-daemon names are
    /// activatable by a service that only leaves a mark (`marker`).
    fn activatable_bus(dir: &Path) -> (PrivateBus, std::path::PathBuf) {
        let services = dir.join("services");
        fs::create_dir_all(&services).unwrap();
        let marker = dir.join("activated");
        for (name, ..) in POWER_PROFILES {
            fs::write(
                services.join(format!("{name}.service")),
                format!(
                    "[D-BUS Service]\nName={name}\nExec=/bin/sh -c 'echo {name} >> {}'\n",
                    marker.display()
                ),
            )
            .unwrap();
        }
        (
            PrivateBus::start_with_services(dir, Some(&services)),
            marker,
        )
    }

    /// The watcher never starts power-profiles-daemon: its names are
    /// activatable here, yet nothing is activated while it follows them;
    /// a plain call does activate (the rig would see it).
    #[test]
    fn the_watcher_never_activates_power_profiles_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let (bus, marker) = activatable_bus(dir.path());
        runtime().block_on(async {
            let cache = ProfileCache::default();
            cache.follow(true);
            let address = bus.address.clone();
            let connects = Arc::new(AtomicU32::new(0));
            let n = Arc::clone(&connects);
            let task = tokio::spawn(watch_profiles(
                cache.clone(),
                || {},
                move || {
                    n.fetch_add(1, Ordering::SeqCst);
                    let a = address.clone();
                    async move { zbus::connection::Builder::address(a.as_str()).build().await }
                },
                Duration::from_millis(10),
            ));
            // The first read (the daemon's start waits for it), then long
            // enough for the subscriptions.
            assert!(cache.follow_and_read(Duration::from_secs(5)).await);
            tokio::time::sleep(Duration::from_millis(500)).await;
            assert_eq!(cache.get(), PowerProfile::Unavailable);
            assert_eq!(connects.load(Ordering::SeqCst), 1, "no failure");
            assert!(
                !marker.exists(),
                "activated: {:?}",
                fs::read_to_string(&marker)
            );
            task.abort();
            // The control: an ordinary call activates.
            let conn = bus.connect().await;
            let (name, path, iface) = POWER_PROFILES[0];
            let props = properties(&conn, name, path).await.unwrap();
            let iface = zbus::names::InterfaceName::try_from(iface).unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(2), props.get(iface, ACTIVE_PROFILE))
                .await;
            let deadline = Instant::now() + Duration::from_secs(5);
            while !marker.exists() {
                assert!(Instant::now() < deadline, "the rig never activates");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
    }

    /// The watcher follows the service: its start (learnt from the owner
    /// change), its profile changes, and its stop; and lets go of it once
    /// no longer followed, following again when asked.
    #[test]
    fn the_watcher_follows_the_service_while_asked_to() {
        let dir = tempfile::tempdir().unwrap();
        let bus = PrivateBus::start_with_services(dir.path(), None);
        runtime().block_on(async {
            let cache = ProfileCache::default();
            let changes = Arc::new(AtomicU32::new(0));
            let c = Arc::clone(&changes);
            let address = bus.address.clone();
            let connects = Arc::new(AtomicU32::new(0));
            let n = Arc::clone(&connects);
            let task = tokio::spawn(watch_profiles(
                cache.clone(),
                move || {
                    c.fetch_add(1, Ordering::SeqCst);
                },
                move || {
                    n.fetch_add(1, Ordering::SeqCst);
                    let a = address.clone();
                    async move { zbus::connection::Builder::address(a.as_str()).build().await }
                },
                Duration::from_millis(10),
            ));
            // Not followed: no connection at all.
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(connects.load(Ordering::SeqCst), 0);
            cache.follow(true);
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(connects.load(Ordering::SeqCst), 1);
            assert_eq!(cache.get(), PowerProfile::Unavailable);

            // The service starts.
            let (name, path, _) = POWER_PROFILES[0];
            let service = bus.connect().await;
            service
                .object_server()
                .at(path, FakeProfiles("power-saver".into()))
                .await
                .unwrap();
            service.request_name(name).await.unwrap();
            until(&cache, &PowerProfile::Active("power-saver".into())).await;
            let seen = changes.load(Ordering::SeqCst);
            assert!(seen >= 1);

            // Its profile changes.
            let iface = service
                .object_server()
                .interface::<_, FakeProfiles>(path)
                .await
                .unwrap();
            iface.get_mut().await.0 = "balanced".into();
            iface
                .get()
                .await
                .active_profile_changed(iface.signal_emitter())
                .await
                .unwrap();
            until(&cache, &PowerProfile::Active("balanced".into())).await;
            assert!(changes.load(Ordering::SeqCst) > seen);

            // It stops.
            service.release_name(name).await.unwrap();
            until(&cache, &PowerProfile::Unavailable).await;

            // No longer followed: the connection goes, and what is read
            // once followed again is fresh.
            service.request_name(name).await.unwrap();
            until(&cache, &PowerProfile::Active("balanced".into())).await;
            cache.follow(false);
            until(&cache, &PowerProfile::Unavailable).await;
            cache.follow(true);
            until(&cache, &PowerProfile::Active("balanced".into())).await;
            assert_eq!(connects.load(Ordering::SeqCst), 2);
            task.abort();
        });
    }

    /// The daemon's start waits for the first read: the running service's
    /// profile is in the cache once the wait returns; a service that is
    /// not running, or a bus that fails, answers at once as unavailable;
    /// a bus that does not answer is waited for only up to the bound.
    #[test]
    fn the_first_read_is_waited_for_within_a_bound() {
        let dir = tempfile::tempdir().unwrap();
        let bus = PrivateBus::start_with_services(dir.path(), None);
        let on_bus = |cache: &ProfileCache| {
            let address = bus.address.clone();
            tokio::spawn(watch_profiles(
                cache.clone(),
                || {},
                move || {
                    let a = address.clone();
                    async move { zbus::connection::Builder::address(a.as_str()).build().await }
                },
                Duration::from_millis(10),
            ))
        };
        let bound = Duration::from_secs(5);
        runtime().block_on(async {
            // No service: read, unavailable.
            let cache = ProfileCache::default();
            let task = on_bus(&cache);
            assert!(cache.follow_and_read(bound).await);
            assert_eq!(cache.get(), PowerProfile::Unavailable);
            task.abort();

            // The service runs on power-saver: read before the wait ends.
            let (name, path, _) = POWER_PROFILES[0];
            let service = bus.connect().await;
            service
                .object_server()
                .at(path, FakeProfiles("power-saver".into()))
                .await
                .unwrap();
            service.request_name(name).await.unwrap();
            let cache = ProfileCache::default();
            let task = on_bus(&cache);
            assert!(cache.follow_and_read(bound).await);
            assert!(cache.followed());
            assert_eq!(cache.get(), PowerProfile::Active("power-saver".into()));
            // Read already: at once.
            let t = Instant::now();
            assert!(cache.follow_and_read(bound).await);
            assert!(t.elapsed() < Duration::from_secs(1));
            // Followed again after a stop: a fresh read is waited for.
            cache.follow(false);
            until(&cache, &PowerProfile::Unavailable).await;
            assert!(cache.follow_and_read(bound).await);
            assert_eq!(cache.get(), PowerProfile::Active("power-saver".into()));
            task.abort();

            // A bus that fails: unavailable at once, retried later.
            let cache = ProfileCache::default();
            let task = tokio::spawn(watch_profiles(
                cache.clone(),
                || {},
                || async { Err(zbus::Error::Failure("no bus".into())) },
                Duration::from_secs(60),
            ));
            let t = Instant::now();
            assert!(cache.follow_and_read(bound).await);
            assert!(t.elapsed() < Duration::from_secs(1));
            assert_eq!(cache.get(), PowerProfile::Unavailable);
            task.abort();

            // A bus that never answers: given up at the bound, the watcher
            // carrying on.
            let cache = ProfileCache::default();
            let task = tokio::spawn(watch_profiles(
                cache.clone(),
                || {},
                std::future::pending::<zbus::Result<Connection>>,
                Duration::from_millis(10),
            ));
            let t = Instant::now();
            assert!(!cache.follow_and_read(Duration::from_millis(100)).await);
            let waited = t.elapsed();
            assert!(waited >= Duration::from_millis(100), "{waited:?}");
            assert!(waited < Duration::from_secs(2), "{waited:?}");
            assert_eq!(cache.get(), PowerProfile::Unavailable);
            assert!(cache.followed());
            assert!(!task.is_finished());
            task.abort();
        });
    }

    /// A bus that cannot be reached is tried again, waiting longer each
    /// time, while followed; then followed as usual.
    #[test]
    fn the_watcher_retries_a_failed_bus_with_a_growing_wait() {
        let dir = tempfile::tempdir().unwrap();
        let bus = PrivateBus::start_with_services(dir.path(), None);
        runtime().block_on(async {
            let cache = ProfileCache::default();
            cache.follow(true);
            let address = bus.address.clone();
            let attempts = Arc::new(Mutex::new(Vec::<Instant>::new()));
            let at = Arc::clone(&attempts);
            let task = tokio::spawn(watch_profiles(
                cache.clone(),
                || {},
                move || {
                    let mut at = at.lock().unwrap();
                    at.push(Instant::now());
                    let fail = at.len() <= 3;
                    let a = address.clone();
                    async move {
                        if fail {
                            return Err(zbus::Error::Failure("no bus".into()));
                        }
                        zbus::connection::Builder::address(a.as_str()).build().await
                    }
                },
                Duration::from_millis(40),
            ));
            let service = bus.connect().await;
            let (name, path, _) = POWER_PROFILES[0];
            service
                .object_server()
                .at(path, FakeProfiles("performance".into()))
                .await
                .unwrap();
            service.request_name(name).await.unwrap();
            until(&cache, &PowerProfile::Active("performance".into())).await;
            let at = attempts.lock().unwrap().clone();
            assert_eq!(at.len(), 4);
            // 40 ms, 80 ms, 160 ms between them.
            for (i, want) in [40u64, 80, 160].into_iter().enumerate() {
                let gap = at[i + 1] - at[i];
                assert!(gap >= Duration::from_millis(want), "{i}: {gap:?}");
            }
            task.abort();
        });
        assert_eq!(RETRY_FIRST, Duration::from_secs(2));
        assert_eq!(RETRY_MAX, Duration::from_secs(60));
    }
}
