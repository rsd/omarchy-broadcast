//! The microphone's hardware capture gain (`[mic] hardware_gain_db`): the
//! capture volume of a USB microphone's ALSA mixer, behind [`Mixer`].
//!
//! * **The device** ([`resolve`]): the source the mic host's capture side
//!   is linked from, when PipeWire says it is; else the configured
//!   `mic.device`, when it names a source; else none. Only a USB audio
//!   device is taken (`alsa.driver_name` `snd_usb_audio`, or `device.bus`
//!   `usb`), and its card is the node's `api.alsa.pcm.card` (else
//!   `alsa.card`).
//! * **The control** ([`choose_control`]): the card's simple elements with a
//!   capture volume; exactly one is taken, several only when one bears a
//!   preferred name ([`PREFERRED`], in that order); else none (the gain is
//!   unavailable: never a guess).
//! * **Its range** is ALSA's, never a table of ours: the dB of the current
//!   value, of the range's ends, and the step (the dB range over the raw
//!   range; the average step, for a control whose dB scale is not linear).
//!   A scale whose lowest value is mute (ALSA's mute sentinel as its
//!   minimum) starts at the raw value above it ([`range_of`]): mute is never
//!   a gain the worker sets.
//! * **Applying** ([`Gain`]): a value is rounded to the nearest step and
//!   clamped to the range ([`quantise`]), set and read back. It is set when
//!   a snapshot brings a value other than the one set last or another
//!   `hardware_gain_serial` (the daemon's count of explicit sets: choosing
//!   the configured value again applies it again), and whenever the device
//!   appears (a new node, a hotplug, the worker or the mic host starting);
//!   never while the value is unset: the device's gain is then left as it
//!   is. A failed write, or a card whose controls could not be listed, is
//!   tried again at the next of these and at the card's next control event:
//!   never on a timer.
//!   Known limit: such a retry happens only when the device, the value or
//!   the apply serial changes, or on an ALSA event from the card; an
//!   unrelated snapshot does not trigger it.
//!   Known limit: when the daemon restarts while the worker keeps running,
//!   the apply serial starts again at 0, so the worker applies the
//!   configured gain once more, overriding a change made meanwhile through
//!   the source volume.
//! * **PipeWire's own writer**: PipeWire's ALSA card profile (ACP) binds
//!   the same element to the physical source's volume (`wpctl set-volume`
//!   moves it, and it moves the source's volume; WirePlumber stores
//!   whatever results). There is no arbitration: the last writer wins.
//!   Setting `hardware_gain_db` therefore pins that source volume: the
//!   worker applies it only while it is set, re-applies it only when the
//!   device appears, and does not fight a later change of the volume there
//!   (`applied` turns false instead).
//! * **External changes** are seen through the card's control events (the
//!   mixer handle's descriptors, polled by the gain thread alongside its
//!   wake-up socket: no timer): the reading is refreshed, and `applied`
//!   says whether the device still holds the value set.
//! * **Threads**: every mixer call runs on the gain thread
//!   (`obc-audio-gain`, [`Link`]), never on the processing thread or the
//!   bus side, which only hand it the device and the value and read the
//!   last reading ([`Published`]: pending until the thread first handled a
//!   device, so that a starting worker does not report "no gain").

use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use obc_core::config::MIC_DEVICE_DEFAULT;

use crate::engine::lock;
use crate::pipewire::AlsaSources;
use crate::status::HardwareGain;
use crate::{debug, info, warning};

/// The capture controls taken, in this order, when a card has several.
pub const PREFERRED: [&str; 4] = ["Mic", "Headset", "Capture", "Microphone"];

/// alsa-lib's dB of a mute value (`SND_CTL_TLV_DB_GAIN_MUTE`), in
/// centibels: a range minimum at or below it is mute.
pub const MUTE_CB: i64 = -9_999_999;

/// What the gain thread publishes: `None` while pending (it has not
/// handled a device yet: the status leaves `hardware_gain` out), else the
/// reading (`Some(None)`: no device or no control, the status's `null`).
pub type Published = Option<Option<HardwareGain>>;

/// The longest card id taken (an index, or an ALSA card id such as
/// `Brio`), in bytes.
pub const CARD_MAX: usize = 32;

/// A capture control, as ALSA reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    /// The simple element's name.
    pub control: String,
    /// The gain now, in dB.
    pub db: f64,
    /// The range's ends, in dB.
    pub min_db: f64,
    pub max_db: f64,
    /// The step, in dB (0 for a control of one value).
    pub step_db: f64,
}

/// What the worker asks of a sound card's mixer. Every call may block for
/// as long as the driver takes (a USB control transfer): only the gain
/// thread makes them.
pub trait Mixer: Send {
    /// The names of the card's simple elements with a capture volume.
    fn capture_controls(&mut self, card: &str) -> Result<Vec<String>, String>;

    /// Reads `control` of `card`.
    fn read(&mut self, card: &str, control: &str) -> Result<Reading, String>;

    /// Sets `control` of `card` to the step nearest `db`, on every channel.
    fn set_db(&mut self, card: &str, control: &str, db: f64) -> Result<(), String>;

    /// The descriptors that become readable when a control of the card
    /// opened last changes (none when nothing is open).
    fn poll_fds(&mut self) -> Vec<libc::pollfd> {
        Vec::new()
    }

    /// Takes the events `fds` (as [`Mixer::poll_fds`] gave them, polled)
    /// say came; whether any did.
    fn events(&mut self, _fds: &[libc::pollfd]) -> bool {
        false
    }

    /// Closes the card opened last (no device now).
    fn release(&mut self) {}
}

/// Whether `card` may name an ALSA card (`hw:<card>`): an index or an id,
/// of letters, digits, `_` and `-`.
pub fn valid_card(card: &str) -> bool {
    !card.is_empty()
        && card.len() <= CARD_MAX
        && card
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The control to use of a card's capture controls `names` (see the module
/// documentation).
pub fn choose_control(names: &[String]) -> Option<&str> {
    match names {
        [one] => Some(one),
        [] => None,
        several => PREFERRED
            .iter()
            .find_map(|p| several.iter().find(|n| n.as_str() == *p))
            .map(String::as_str),
    }
}

/// `db` on the control `r` reads: clamped to its range and rounded to its
/// nearest step from the minimum. `None` for a value that is not finite.
pub fn quantise(db: f64, r: &Reading) -> Option<f64> {
    if !db.is_finite() {
        return None;
    }
    let (lo, hi) = (r.min_db.min(r.max_db), r.max_db.max(r.min_db));
    let clamped = db.clamp(lo, hi);
    if !(r.step_db.is_finite() && r.step_db > 0.0) {
        return Some(clamped);
    }
    let steps = ((clamped - lo) / r.step_db).round();
    Some((lo + steps * r.step_db).clamp(lo, hi))
}

/// A control's range in dB, `(min, max, step)`, from its raw range `lo..=hi`
/// and the dB of its ends `dlo`, `dhi` (centibels); `db_at` is the dB of a
/// raw value. A minimum that is mute ([`MUTE_CB`]) is replaced by the dB of
/// `lo + 1`, and the step counted from there.
pub fn range_of(
    lo: i64,
    hi: i64,
    dlo: i64,
    dhi: i64,
    db_at: impl Fn(i64) -> Option<i64>,
) -> (f64, f64, f64) {
    let (lo, dlo) = if dlo <= MUTE_CB && hi > lo {
        match db_at(lo + 1).filter(|v| *v > MUTE_CB) {
            Some(v) => (lo + 1, v),
            None => (lo, dlo),
        }
    } else {
        (lo, dlo)
    };
    let (min_db, max_db) = (dlo as f64 / 100.0, dhi as f64 / 100.0);
    let step_db = if hi > lo {
        (max_db - min_db) / (hi - lo) as f64
    } else {
        0.0
    };
    (min_db, max_db, step_db)
}

/// The dB to report for a current value of `cb` centibels: a value at or
/// below ALSA's mute sentinel ([`MUTE_CB`]) is reported as `min_db`, the
/// lowest gain the worker sets, so that the reading stays a real dB.
pub fn reported_db(cb: i64, min_db: f64) -> f64 {
    if cb <= MUTE_CB {
        min_db
    } else {
        cb as f64 / 100.0
    }
}

/// The device the gain is set on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Its ALSA card index.
    pub card: String,
    /// Its node's `node.name`.
    pub node: String,
    /// Its node's id: another one is another appearance of the device.
    pub node_id: u32,
    /// The mic host's capture node, while it runs: another one (the mic
    /// host started again) is another appearance too.
    pub capture: Option<u32>,
}

/// The device of the hardware gain (see the module documentation):
/// `configured` is the snapshot's `device`.
pub fn resolve(sources: &AlsaSources, configured: &str) -> Option<Target> {
    let name = match sources.captured.as_deref() {
        Some(n) => n,
        None if configured.is_empty() || configured == MIC_DEVICE_DEFAULT => return None,
        None => configured,
    };
    let source = sources.sources.iter().find(|s| s.name == name)?;
    if !source.usb {
        return None;
    }
    Some(Target {
        card: source.card.clone().filter(|c| valid_card(c))?,
        node: source.name.clone(),
        node_id: source.id,
        capture: sources.capture,
    })
}

/// The hardware gain, applied and read (see the module documentation).
pub struct Gain {
    mixer: Box<dyn Mixer>,
    target: Option<Target>,
    /// The target's control, when it has one.
    control: Option<String>,
    /// The target's controls could not be listed: tried again.
    unlisted: bool,
    /// The daemon's serial of the last value handed in.
    serial: u64,
    /// The value wanted.
    want: Option<f64>,
    /// The value set last on the target (as wanted, before rounding), and
    /// whether the write succeeded.
    set: Option<(f64, bool)>,
    report: Option<HardwareGain>,
}

impl Gain {
    /// No device yet, nothing set.
    pub fn new(mixer: Box<dyn Mixer>) -> Gain {
        Gain {
            mixer,
            target: None,
            control: None,
            unlisted: false,
            serial: 0,
            want: None,
            set: None,
            report: None,
        }
    }

    /// The reading for the status: `None` with no device or no control.
    pub fn report(&self) -> Option<&HardwareGain> {
        self.report.as_ref()
    }

    /// The device, the value wanted now and the daemon's serial of it: the
    /// value is set when the device appeared (another target), the value
    /// differs from the one set last, the serial is another (an explicit
    /// set of the same value) or the last write failed; never when it is
    /// `None`. Returns whether the reading changed.
    pub fn update(&mut self, target: Option<Target>, want: Option<f64>, serial: u64) -> bool {
        let appeared = target != self.target;
        if appeared {
            self.target = target;
            self.set = None;
            self.unlisted = false;
            self.control = match self.target.clone() {
                Some(t) => self.find_control(&t),
                None => {
                    self.mixer.release();
                    None
                }
            };
        } else if self.unlisted
            && let Some(t) = self.target.clone()
        {
            self.control = self.find_control(&t);
        }
        let again = serial != self.serial;
        self.serial = serial;
        self.want = want;
        match (self.target.clone(), self.control.clone(), want) {
            (Some(t), Some(control), Some(w)) => {
                if again || self.set != Some((w, true)) {
                    let ok = self.write(&t, &control, w);
                    self.set = Some((w, ok));
                }
            }
            (_, _, None) => self.set = None,
            _ => {}
        }
        self.refresh()
    }

    /// A control event came: a write that failed is tried again, and the
    /// control read again. Returns whether the reading changed.
    pub fn on_event(&mut self) -> bool {
        if let (Some(t), Some(control), Some(w), Some((v, false))) = (
            self.target.clone(),
            self.control.clone(),
            self.want,
            self.set,
        ) && v == w
        {
            let ok = self.write(&t, &control, w);
            self.set = Some((w, ok));
        }
        self.refresh()
    }

    /// Reads the control again. Returns whether the reading changed.
    pub fn refresh(&mut self) -> bool {
        let next = match (&self.target, &self.control) {
            (Some(t), Some(c)) => match self.mixer.read(&t.card, c) {
                Ok(r) => Some(self.status_of(r)),
                Err(e) => {
                    warning!(
                        "the hardware gain of {} cannot be read: {e}",
                        describe(t, c)
                    );
                    None
                }
            },
            _ => None,
        };
        let changed = next != self.report;
        self.report = next;
        changed
    }

    /// The descriptors of the card's control events.
    pub fn poll_fds(&mut self) -> Vec<libc::pollfd> {
        if self.control.is_none() {
            return Vec::new();
        }
        self.mixer.poll_fds()
    }

    /// Takes the control events `fds` say came; whether any did.
    pub fn events(&mut self, fds: &[libc::pollfd]) -> bool {
        self.mixer.events(fds)
    }

    fn status_of(&self, r: Reading) -> HardwareGain {
        // The device holds the value set (the step nearest it): an
        // external change since, by a step or more, is not ours. ALSA
        // gives dB in hundredths: a fine step is matched to those.
        let applied = match (self.want, self.set) {
            (Some(w), Some((v, true))) if v == w => {
                quantise(w, &r).is_some_and(|q| (q - r.db).abs() < (r.step_db / 2.0).max(0.01))
            }
            _ => false,
        };
        HardwareGain {
            control: r.control,
            db: r.db,
            min_db: r.min_db,
            max_db: r.max_db,
            step_db: r.step_db,
            applied,
        }
    }

    /// The control of `t`; [`Gain::unlisted`] set when its controls
    /// cannot be listed.
    fn find_control(&mut self, t: &Target) -> Option<String> {
        let listed = self.mixer.capture_controls(&t.card);
        self.unlisted = listed.is_err();
        match listed {
            Ok(names) => match choose_control(&names) {
                Some(c) => {
                    debug!("the hardware gain of {} is its control {c:?}", t.node);
                    Some(c.to_string())
                }
                None => {
                    info!(
                        "{} (ALSA card {}) has no single capture control ({}): its hardware \
                         gain is not available",
                        t.node,
                        t.card,
                        if names.is_empty() {
                            "none".to_string()
                        } else {
                            names.join(", ")
                        }
                    );
                    None
                }
            },
            Err(e) => {
                warning!(
                    "the mixer of {} (ALSA card {}) cannot be read: {e}",
                    t.node,
                    t.card
                );
                None
            }
        }
    }

    /// Sets `want` (rounded, clamped) on `control`; whether it was set.
    fn write(&mut self, t: &Target, control: &str, want: f64) -> bool {
        let r = match self.mixer.read(&t.card, control) {
            Ok(r) => r,
            Err(e) => {
                warning!(
                    "the hardware gain of {} cannot be read: {e}",
                    describe(t, control)
                );
                return false;
            }
        };
        let Some(db) = quantise(want, &r) else {
            return false;
        };
        match self.mixer.set_db(&t.card, control, db) {
            Ok(()) => {
                info!(
                    "the hardware gain of {} set to {db} dB (asked {want} dB, range {} to {} dB)",
                    describe(t, control),
                    r.min_db,
                    r.max_db
                );
                true
            }
            Err(e) => {
                warning!(
                    "the hardware gain of {} cannot be set to {db} dB: {e}",
                    describe(t, control)
                );
                false
            }
        }
    }
}

/// `node (card N, control)`.
fn describe(t: &Target, control: &str) -> String {
    format!("{} (ALSA card {}, {control})", t.node, t.card)
}

/// What [`Link::send`] hands the gain thread: the device, the value wanted
/// and the daemon's serial of it.
type Wanted = (Option<Target>, Option<f64>, u64);

/// The gain thread: it owns the [`Gain`], takes the device and the value
/// from [`Link::send`], follows the card's control events, and publishes the
/// reading to `out` ([`Published`]: pending until the first device handed
/// in, and again once it stops). Dropped, it stops (and is joined).
pub struct Link {
    tx: Option<Sender<Wanted>>,
    wake: Option<UnixStream>,
    handle: Option<JoinHandle<()>>,
}

impl Link {
    /// Starts the thread over `mixer`.
    pub fn spawn(mixer: Box<dyn Mixer>, out: Arc<Mutex<Published>>) -> std::io::Result<Link> {
        let (wake, wake_rx) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        wake_rx.set_nonblocking(true)?;
        let (tx, rx) = channel::<Wanted>();
        let handle = std::thread::Builder::new()
            .name("obc-audio-gain".into())
            .spawn(move || {
                let mut gain = Gain::new(mixer);
                let mut wake_rx = wake_rx;
                let mut buf = [0u8; 64];
                let mut handled = false;
                loop {
                    let mut fds = vec![obc_audio::sys::pollfd(wake_rx.as_fd(), libc::POLLIN)];
                    fds.extend(gain.poll_fds());
                    if let Err(e) = obc_audio::sys::poll(&mut fds, None) {
                        warning!("the hardware gain thread cannot wait: {e}");
                        break;
                    }
                    let mut changed = false;
                    let mut closed = false;
                    if fds[0].revents != 0 {
                        loop {
                            match wake_rx.read(&mut buf) {
                                Ok(0) => {
                                    closed = true;
                                    break;
                                }
                                Ok(_) => {}
                                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                                Err(_) => {
                                    closed = true;
                                    break;
                                }
                            }
                        }
                        let mut last = None;
                        loop {
                            match rx.try_recv() {
                                Ok(m) => last = Some(m),
                                Err(TryRecvError::Empty) => break,
                                Err(TryRecvError::Disconnected) => {
                                    closed = true;
                                    break;
                                }
                            }
                        }
                        if closed {
                            break;
                        }
                        if let Some((target, want, serial)) = last {
                            changed |= gain.update(target, want, serial);
                            // The first reading, even "none", ends the
                            // pending one.
                            changed |= !handled;
                            handled = true;
                        }
                    }
                    if fds.len() > 1 && gain.events(&fds[1..]) {
                        changed |= gain.on_event();
                    }
                    if changed {
                        *lock(&out) = Some(gain.report().cloned());
                    }
                }
                *lock(&out) = None;
            })?;
        Ok(Link {
            tx: Some(tx),
            wake: Some(wake),
            handle: Some(handle),
        })
    }

    /// Hands the thread the device, the value wanted now and the daemon's
    /// serial of it.
    pub fn send(&self, target: Option<Target>, want: Option<f64>, serial: u64) {
        if let Some(tx) = &self.tx
            && tx.send((target, want, serial)).is_ok()
            && let Some(mut w) = self.wake.as_ref()
        {
            // A full socket is a wake-up already pending.
            let _ = w.write(&[1]);
        }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        drop(self.tx.take());
        drop(self.wake.take());
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// The [`Mixer`] over alsa-lib's simple mixer API: one card open at a time
/// (the last one used), whose control events [`Mixer::poll_fds`] gives.
/// Only a control of index 0 is taken.
#[derive(Default)]
pub struct Alsa {
    open: Option<(String, alsa::mixer::Mixer)>,
}

impl std::fmt::Debug for Alsa {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Alsa")
            .field("card", &self.open.as_ref().map(|(c, _)| c))
            .finish()
    }
}

impl Alsa {
    /// The mixer of `card`, opened (non-blocking, for its events) unless it
    /// is the one open.
    fn mixer(&mut self, card: &str) -> Result<&alsa::mixer::Mixer, String> {
        if !valid_card(card) {
            return Err(format!("{card:?} is not an ALSA card"));
        }
        if self.open.as_ref().is_none_or(|(c, _)| c != card) {
            self.open = None;
            let m = alsa::mixer::Mixer::new(&format!("hw:{card}"), true)
                .map_err(|e| format!("cannot open the mixer of ALSA card {card}: {e}"))?;
            self.open = Some((card.to_string(), m));
        }
        let (_, m) = self.open.as_ref().expect("opened");
        // Changes made elsewhere since: into the element cache.
        let _ = m.handle_events();
        Ok(m)
    }
}

/// The capture element `control` of `m`.
fn selem<'a>(m: &'a alsa::mixer::Mixer, control: &str) -> Result<alsa::mixer::Selem<'a>, String> {
    m.find_selem(&alsa::mixer::SelemId::new(control, 0))
        .filter(alsa::mixer::Selem::has_capture_volume)
        .ok_or_else(|| format!("there is no capture control {control:?}"))
}

/// The first capture channel of `s`.
fn capture_channel(s: &alsa::mixer::Selem<'_>) -> Option<alsa::mixer::SelemChannelId> {
    use alsa::mixer::SelemChannelId as C;
    [
        C::mono(),
        C::FrontRight,
        C::RearLeft,
        C::RearRight,
        C::FrontCenter,
        C::Woofer,
        C::SideLeft,
        C::SideRight,
        C::RearCenter,
    ]
    .into_iter()
    .find(|c| s.has_capture_channel(*c))
}

/// Centibels (alsa-lib's "dB" unit) in dB.
impl Mixer for Alsa {
    fn capture_controls(&mut self, card: &str) -> Result<Vec<String>, String> {
        let m = self.mixer(card)?;
        Ok(m.iter()
            .filter_map(alsa::mixer::Selem::new)
            .filter(|s| s.has_capture_volume())
            .filter_map(|s| {
                let id = s.get_id();
                (id.get_index() == 0)
                    .then(|| id.get_name().ok().map(str::to_string))
                    .flatten()
            })
            .collect())
    }

    fn read(&mut self, card: &str, control: &str) -> Result<Reading, String> {
        let m = self.mixer(card)?;
        let s = selem(m, control)?;
        let channel = capture_channel(&s)
            .ok_or_else(|| format!("the control {control:?} has no capture channel"))?;
        let (lo, hi) = s.get_capture_volume_range();
        let (dlo, dhi) = s.get_capture_db_range();
        let raw = s
            .get_capture_volume(channel)
            .map_err(|e| format!("cannot read {control:?}: {e}"))?;
        let db = s
            .ask_capture_vol_db(raw)
            .map_err(|e| format!("{control:?} has no dB scale: {e}"))?;
        let (min_db, max_db, step_db) = range_of(lo, hi, dlo.0, dhi.0, |raw| {
            s.ask_capture_vol_db(raw).ok().map(|v| v.0)
        });
        Ok(Reading {
            control: control.to_string(),
            db: reported_db(db.0, min_db),
            min_db,
            max_db,
            step_db,
        })
    }

    fn set_db(&mut self, card: &str, control: &str, db: f64) -> Result<(), String> {
        let m = self.mixer(card)?;
        let s = selem(m, control)?;
        let mb = alsa::mixer::MilliBel((db * 100.0).round() as i64);
        // The raw value whose dB is nearest: below or above.
        let below = s.ask_capture_db_vol(mb, alsa::Round::Floor);
        let above = s.ask_capture_db_vol(mb, alsa::Round::Ceil);
        let distance = |raw: i64| {
            s.ask_capture_vol_db(raw)
                .map(|v| (v.0 - mb.0).abs())
                .unwrap_or(i64::MAX)
        };
        let raw = match (below, above) {
            (Ok(b), Ok(a)) => {
                if distance(a) < distance(b) {
                    a
                } else {
                    b
                }
            }
            (Ok(v), Err(_)) | (Err(_), Ok(v)) => v,
            (Err(e), Err(_)) => return Err(format!("{control:?} has no dB scale: {e}")),
        };
        s.set_capture_volume_all(raw)
            .map_err(|e| format!("cannot set {control:?}: {e}"))
    }

    fn poll_fds(&mut self) -> Vec<libc::pollfd> {
        self.open
            .as_ref()
            .and_then(|(_, m)| alsa::poll::Descriptors::get(m).ok())
            .unwrap_or_default()
    }

    fn events(&mut self, fds: &[libc::pollfd]) -> bool {
        let Some((card, m)) = self.open.as_ref() else {
            return false;
        };
        let flags = match alsa::poll::Descriptors::revents(m, fds) {
            Ok(f) => f,
            Err(_) => alsa::poll::Flags::ERR,
        };
        if flags.is_empty() {
            return false;
        }
        if flags
            .intersects(alsa::poll::Flags::ERR | alsa::poll::Flags::HUP | alsa::poll::Flags::NVAL)
            || m.handle_events().is_err()
        {
            // The card went (unplugged): closed, so that its descriptors
            // are not polled again; the next use opens it anew.
            debug!("the mixer of ALSA card {card} went: closed");
            self.open = None;
        }
        true
    }

    fn release(&mut self) {
        self.open = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipewire::AlsaSource;
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    /// A control of the fake: raw `lo..=hi`, linear from `min_db` in
    /// steps of `step_db`.
    #[derive(Debug, Clone)]
    struct Control {
        raw: i64,
        lo: i64,
        hi: i64,
        min_db: f64,
        step_db: f64,
    }

    impl Control {
        fn db(&self, raw: i64) -> f64 {
            self.min_db + (raw - self.lo) as f64 * self.step_db
        }
    }

    #[derive(Debug, Default)]
    struct State {
        /// Card: (control name: control), in name order.
        cards: BTreeMap<String, BTreeMap<String, Control>>,
        /// Every `set_db`: (card, control, dB).
        sets: Vec<(String, String, f64)>,
        releases: usize,
        /// The next `set_db` calls that fail.
        fail_sets: usize,
        /// The next `capture_controls` calls that fail.
        fail_lists: usize,
    }

    /// A mixer over [`State`], shared with the test.
    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<State>>);

    impl Fake {
        /// The MX Brio's control: `Headset`, 0..=36 for +18..+54 dB, at 22.
        fn brio() -> Fake {
            let f = Fake::default();
            f.add("0", "Headset", 22, 0, 36, 18.0, 1.0);
            f
        }

        #[allow(clippy::too_many_arguments)]
        fn add(
            &self,
            card: &str,
            name: &str,
            raw: i64,
            lo: i64,
            hi: i64,
            min_db: f64,
            step_db: f64,
        ) {
            lock(&self.0).cards.entry(card.into()).or_default().insert(
                name.into(),
                Control {
                    raw,
                    lo,
                    hi,
                    min_db,
                    step_db,
                },
            );
        }

        /// Another writer (PipeWire) moves the control.
        fn set_raw(&self, card: &str, name: &str, raw: i64) {
            lock(&self.0)
                .cards
                .get_mut(card)
                .unwrap()
                .get_mut(name)
                .unwrap()
                .raw = raw;
        }

        fn db(&self, card: &str, name: &str) -> f64 {
            let s = lock(&self.0);
            let c = &s.cards[card][name];
            c.db(c.raw)
        }

        fn sets(&self) -> Vec<(String, String, f64)> {
            lock(&self.0).sets.clone()
        }
    }

    impl Mixer for Fake {
        fn capture_controls(&mut self, card: &str) -> Result<Vec<String>, String> {
            let mut s = lock(&self.0);
            if s.fail_lists > 0 {
                s.fail_lists -= 1;
                return Err("the card is busy".into());
            }
            s.cards
                .get(card)
                .map(|c| c.keys().cloned().collect())
                .ok_or_else(|| format!("no card {card}"))
        }

        fn read(&mut self, card: &str, control: &str) -> Result<Reading, String> {
            let s = lock(&self.0);
            let c = s
                .cards
                .get(card)
                .and_then(|c| c.get(control))
                .ok_or("no such control")?;
            Ok(Reading {
                control: control.into(),
                db: c.db(c.raw),
                min_db: c.db(c.lo),
                max_db: c.db(c.hi),
                step_db: c.step_db,
            })
        }

        fn set_db(&mut self, card: &str, control: &str, db: f64) -> Result<(), String> {
            let mut s = lock(&self.0);
            s.sets.push((card.into(), control.into(), db));
            if s.fail_sets > 0 {
                s.fail_sets -= 1;
                return Err("a USB control transfer failed".into());
            }
            let c = s
                .cards
                .get_mut(card)
                .and_then(|c| c.get_mut(control))
                .ok_or("no such control")?;
            c.raw = (c.lo + ((db - c.min_db) / c.step_db).round() as i64).clamp(c.lo, c.hi);
            Ok(())
        }

        fn release(&mut self) {
            lock(&self.0).releases += 1;
        }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn target(card: &str, node_id: u32) -> Target {
        Target {
            card: card.into(),
            node: "alsa_input.usb-brio".into(),
            node_id,
            capture: Some(90),
        }
    }

    fn reading(min_db: f64, max_db: f64, step_db: f64) -> Reading {
        Reading {
            control: "Headset".into(),
            db: min_db,
            min_db,
            max_db,
            step_db,
        }
    }

    #[test]
    fn one_capture_control_or_a_preferred_name_else_none() {
        assert_eq!(choose_control(&names(&["Headset"])), Some("Headset"));
        assert_eq!(choose_control(&names(&["Line"])), Some("Line"));
        assert_eq!(choose_control(&names(&["Capture", "Mic"])), Some("Mic"));
        assert_eq!(
            choose_control(&names(&["Microphone", "Capture", "Headset"])),
            Some("Headset")
        );
        assert_eq!(choose_control(&names(&["Line", "Aux"])), None);
        assert_eq!(choose_control(&names(&["mic", "Mic Boost"])), None);
        assert_eq!(choose_control(&[]), None);
    }

    #[test]
    fn a_value_is_rounded_to_the_step_and_clamped_to_the_range() {
        let brio = reading(18.0, 54.0, 1.0);
        assert_eq!(quantise(40.0, &brio), Some(40.0));
        assert_eq!(quantise(40.4, &brio), Some(40.0));
        assert_eq!(quantise(40.6, &brio), Some(41.0));
        assert_eq!(quantise(100.0, &brio), Some(54.0));
        assert_eq!(quantise(-5.0, &brio), Some(18.0));
        assert_eq!(quantise(f64::NAN, &brio), None);
        assert_eq!(quantise(f64::INFINITY, &brio), None);
        // A step that is not a whole dB, counted from the minimum.
        let fine = reading(-12.0, 33.0, 0.375);
        assert_eq!(quantise(0.2, &fine), Some(0.375));
        assert_eq!(quantise(0.1, &fine), Some(0.0));
        assert_eq!(quantise(40.0, &fine), Some(33.0));
        // One value: clamped only.
        assert_eq!(quantise(3.0, &reading(6.0, 6.0, 0.0)), Some(6.0));
    }

    fn source(id: u32, name: &str, card: Option<&str>, usb: bool) -> AlsaSource {
        AlsaSource {
            id,
            name: name.into(),
            card: card.map(str::to_string),
            usb,
        }
    }

    #[test]
    fn the_device_is_the_captured_one_else_the_configured_one_and_usb_only() {
        let sources = AlsaSources {
            capture: Some(90),
            captured: None,
            sources: vec![
                source(33, "brio", Some("0"), true),
                source(46, "pci-mic", Some("1"), false),
                source(50, "virtual", None, true),
            ],
        };
        // Not captured (the mic host does not run): the configured one.
        assert_eq!(
            resolve(&sources, "brio"),
            Some(Target {
                card: "0".into(),
                node: "brio".into(),
                node_id: 33,
                capture: Some(90),
            })
        );
        assert_eq!(resolve(&sources, "default"), None);
        assert_eq!(resolve(&sources, ""), None);
        assert_eq!(resolve(&sources, "unplugged"), None);
        assert_eq!(resolve(&sources, "pci-mic"), None, "not USB");
        assert_eq!(resolve(&sources, "virtual"), None, "no ALSA card");
        // Captured: what the capture side is linked from wins.
        let captured = AlsaSources {
            captured: Some("brio".into()),
            ..sources.clone()
        };
        assert_eq!(resolve(&captured, "default").unwrap().node_id, 33);
        assert_eq!(resolve(&captured, "pci-mic").unwrap().node, "brio");
        let captured_pci = AlsaSources {
            captured: Some("pci-mic".into()),
            ..sources
        };
        assert_eq!(resolve(&captured_pci, "brio"), None, "never another device");
    }

    #[test]
    fn unset_the_device_is_read_and_left_as_it_is() {
        let fake = Fake::brio();
        let mut gain = Gain::new(Box::new(fake.clone()));
        assert!(gain.update(Some(target("0", 33)), None, 0));
        assert!(fake.sets().is_empty());
        assert_eq!(
            gain.report(),
            Some(&HardwareGain {
                control: "Headset".into(),
                db: 40.0,
                min_db: 18.0,
                max_db: 54.0,
                step_db: 1.0,
                applied: false,
            })
        );
        // The device again, with another node: still nothing set.
        gain.update(Some(target("0", 34)), None, 0);
        assert!(fake.sets().is_empty());
        assert_eq!(fake.db("0", "Headset"), 40.0);
    }

    #[test]
    fn a_value_is_set_on_change_and_on_appearance_only() {
        let fake = Fake::brio();
        let mut gain = Gain::new(Box::new(fake.clone()));
        let brio = Some(target("0", 33));
        assert!(gain.update(brio.clone(), Some(30.4), 0));
        assert_eq!(fake.sets(), [("0".into(), "Headset".into(), 30.0)]);
        assert_eq!(fake.db("0", "Headset"), 30.0);
        let r = gain.report().unwrap();
        assert_eq!((r.db, r.applied), (30.0, true));
        // The same snapshot again: nothing set.
        assert!(!gain.update(brio.clone(), Some(30.4), 0));
        assert_eq!(fake.sets().len(), 1);
        // Another value: set, clamped.
        gain.update(brio.clone(), Some(80.0), 0);
        assert_eq!(fake.sets().len(), 2);
        assert_eq!(fake.db("0", "Headset"), 54.0);
        assert!(gain.report().unwrap().applied);
        // PipeWire moves it: seen at the next reading, not undone.
        fake.set_raw("0", "Headset", 10);
        assert!(gain.refresh());
        let r = gain.report().unwrap();
        assert_eq!((r.db, r.applied), (28.0, false));
        assert!(!gain.update(brio.clone(), Some(80.0), 0));
        assert_eq!(fake.sets().len(), 2);
        // The device appears again (a hotplug: another node): set again.
        gain.update(Some(target("0", 70)), Some(80.0), 0);
        assert_eq!(fake.sets().len(), 3);
        assert!(gain.report().unwrap().applied);
        // The mic host starts again (another capture node): set again.
        let restarted = Target {
            capture: Some(91),
            ..target("0", 70)
        };
        gain.update(Some(restarted.clone()), Some(80.0), 0);
        assert_eq!(fake.sets().len(), 4);
        // Unset: the device keeps what it has.
        gain.update(Some(restarted.clone()), None, 0);
        assert_eq!(fake.sets().len(), 4);
        assert_eq!(fake.db("0", "Headset"), 54.0);
        assert!(!gain.report().unwrap().applied);
        // Set again to the value of before: it is a change.
        gain.update(Some(restarted), Some(80.0), 0);
        assert_eq!(fake.sets().len(), 5);
    }

    #[test]
    fn no_device_or_no_single_control_is_no_gain() {
        let fake = Fake::brio();
        fake.add("3", "Line", 0, 0, 10, 0.0, 1.0);
        fake.add("3", "Aux", 0, 0, 10, 0.0, 1.0);
        fake.add("4", "Capture", 3, 0, 10, -6.0, 1.5);
        fake.add("4", "Mic", 11, 0, 16, 0.0, 1.5);
        let mut gain = Gain::new(Box::new(fake.clone()));
        gain.update(Some(target("3", 40)), Some(10.0), 0);
        assert_eq!(gain.report(), None, "ambiguous: never a guess");
        assert!(fake.sets().is_empty());
        gain.update(Some(target("9", 41)), Some(10.0), 0);
        assert_eq!(gain.report(), None, "no such card");
        // A preferred name among several.
        gain.update(Some(target("4", 42)), Some(10.0), 0);
        assert_eq!(fake.sets(), [("4".into(), "Mic".into(), 10.5)]);
        assert_eq!(gain.report().unwrap().control, "Mic");
        // The device goes: no reading, the card let go.
        assert!(gain.update(None, Some(10.0), 0));
        assert_eq!(gain.report(), None);
        assert_eq!(lock(&fake.0).releases, 1);
    }

    #[test]
    fn the_status_carries_the_reading_or_null() {
        let mut status = crate::status::Status::default();
        let v: serde_json::Value = serde_json::from_str(&status.to_json()).unwrap();
        assert!(v.get("hardware_gain").is_none(), "pending: left out");
        status.hardware_gain = Some(None);
        let v: serde_json::Value = serde_json::from_str(&status.to_json()).unwrap();
        assert_eq!(v["hardware_gain"], serde_json::Value::Null);
        status.hardware_gain = Some(Some(HardwareGain {
            control: "Headset".into(),
            db: 40.0,
            min_db: 18.0,
            max_db: 54.0,
            step_db: 1.0,
            applied: true,
        }));
        let v: serde_json::Value = serde_json::from_str(&status.to_json()).unwrap();
        assert_eq!(
            v["hardware_gain"],
            serde_json::json!({"control": "Headset", "db": 40.0, "min_db": 18.0,
                               "max_db": 54.0, "step_db": 1.0, "applied": true})
        );
        // A change of the reading is a change of the status (StatusChanged).
        let before = status.without_counters();
        status.hardware_gain.as_mut().unwrap().as_mut().unwrap().db = 41.0;
        assert_ne!(before, status.without_counters());
    }

    #[test]
    fn the_thread_applies_publishes_and_stops() {
        let fake = Fake::brio();
        let out: Arc<Mutex<Published>> = Arc::default();
        let link = Link::spawn(Box::new(fake.clone()), out.clone()).unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        let wait = |what: &str, done: &dyn Fn(&Published) -> bool| {
            while !done(&lock(&out)) {
                assert!(Instant::now() < end, "never {what}: {:?}", lock(&out));
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        // Nothing handed in yet: pending, not "no gain".
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(*lock(&out), None);
        // No device: "no gain" (null), no longer pending.
        link.send(None, Some(35.0), 0);
        wait("no gain", &|p| *p == Some(None));
        link.send(Some(target("0", 33)), Some(35.0), 0);
        wait("applied", &|p| {
            p.as_ref()
                .is_some_and(|g| g.as_ref().is_some_and(|g| g.applied))
        });
        assert_eq!(fake.db("0", "Headset"), 35.0);
        // PipeWire moves it; the same value set again (another serial):
        // applied again.
        fake.set_raw("0", "Headset", 10);
        link.send(Some(target("0", 33)), Some(35.0), 1);
        wait("set twice", &|_| fake.sets().len() == 2);
        assert_eq!(fake.db("0", "Headset"), 35.0);
        link.send(None, Some(35.0), 1);
        wait("cleared", &|p| *p == Some(None));
        let started = Instant::now();
        drop(link);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(*lock(&out), None, "stopped: pending again");
        assert_eq!(fake.sets().len(), 2);
    }

    #[test]
    fn the_same_value_set_again_is_applied_again() {
        // The reviewer's case: 40 set, moved to 28 elsewhere, 40 chosen
        // again (the config text does not change, the serial does).
        let fake = Fake::brio();
        let mut gain = Gain::new(Box::new(fake.clone()));
        let brio = Some(target("0", 33));
        gain.update(brio.clone(), Some(40.0), 4);
        assert_eq!(fake.sets().len(), 1);
        fake.set_raw("0", "Headset", 10);
        gain.refresh();
        assert!(!gain.report().unwrap().applied);
        // The same serial (another key of the snapshot changed): left.
        gain.update(brio.clone(), Some(40.0), 4);
        assert_eq!(fake.sets().len(), 1);
        assert_eq!(fake.db("0", "Headset"), 28.0);
        // Another serial: set again.
        assert!(gain.update(brio.clone(), Some(40.0), 5));
        assert_eq!(fake.sets().len(), 2);
        let r = gain.report().unwrap();
        assert_eq!((r.db, r.applied), (40.0, true));
        // A serial while unset writes nothing.
        gain.update(brio, None, 6);
        assert_eq!(fake.sets().len(), 2);
    }

    #[test]
    fn a_failed_write_or_listing_is_tried_again_at_the_next_turn() {
        let fake = Fake::brio();
        let mut gain = Gain::new(Box::new(fake.clone()));
        let brio = Some(target("0", 33));
        lock(&fake.0).fail_sets = 1;
        gain.update(brio.clone(), Some(30.0), 0);
        assert_eq!(fake.db("0", "Headset"), 40.0);
        assert!(!gain.report().unwrap().applied);
        // A control event (or the next snapshot): tried again, once.
        gain.on_event();
        assert_eq!(fake.db("0", "Headset"), 30.0);
        assert!(gain.report().unwrap().applied);
        gain.on_event();
        gain.update(brio, Some(30.0), 0);
        assert_eq!(fake.sets().len(), 2, "never again once set");

        // The card's controls cannot be listed: listed again at the next
        // snapshot, not before.
        let fake = Fake::brio();
        let mut gain = Gain::new(Box::new(fake.clone()));
        lock(&fake.0).fail_lists = 1;
        let brio = Some(target("0", 33));
        gain.update(brio.clone(), Some(30.0), 0);
        assert_eq!(gain.report(), None);
        assert!(gain.poll_fds().is_empty());
        gain.update(brio, Some(30.0), 0);
        assert_eq!(fake.db("0", "Headset"), 30.0);
        assert!(gain.report().unwrap().applied);
    }

    #[test]
    fn a_fine_step_is_applied_within_alsa_hundredths() {
        // A step of 0.0625 dB: ALSA reads the value back in hundredths.
        let r = Reading {
            control: "Mic".into(),
            db: 10.06,
            min_db: 0.0,
            max_db: 20.0,
            step_db: 0.0625,
        };
        let fake = Fake::default();
        fake.add("5", "Mic", 0, 0, 320, 0.0, 0.0625);
        let mut gain = Gain::new(Box::new(fake));
        gain.update(Some(target("5", 50)), Some(10.0625), 0);
        assert!(gain.status_of(r).applied);
    }

    #[test]
    fn a_mute_minimum_is_measured_from_the_value_above() {
        // 0 is mute, 1..=36 are -18..+17 dB (centibels).
        let db_at = |raw: i64| Some(if raw == 0 { MUTE_CB } else { -1900 + raw * 100 });
        let (min, max, step) = range_of(0, 36, MUTE_CB, 1700, db_at);
        assert_eq!((min, max, step), (-18.0, 17.0, 1.0));
        let r = Reading {
            control: "Mic".into(),
            db: 0.0,
            min_db: min,
            max_db: max,
            step_db: step,
        };
        assert_eq!(quantise(40.0, &r), Some(17.0));
        assert_eq!(quantise(-50.0, &r), Some(-18.0), "never mute");
        // No mute: as ALSA says.
        assert_eq!(range_of(0, 36, 1800, 5400, |_| None), (18.0, 54.0, 1.0));
        // One value, mute: left as it is.
        assert_eq!(range_of(0, 0, MUTE_CB, 0, |_| None).2, 0.0);
    }

    #[test]
    fn a_muted_current_value_is_reported_as_the_minimum() {
        assert_eq!(reported_db(MUTE_CB, -18.0), -18.0);
        assert_eq!(reported_db(MUTE_CB - 5, -18.0), -18.0);
        assert_eq!(reported_db(-1800, -18.0), -18.0);
        assert_eq!(reported_db(1250, -18.0), 12.5);
    }

    #[test]
    fn a_card_is_an_index_or_an_id() {
        assert!(valid_card("0") && valid_card("Brio") && valid_card("USB_Audio-2"));
        assert!(!valid_card("") && !valid_card("hw:0") && !valid_card("0,1"));
        assert!(!valid_card(&"x".repeat(CARD_MAX + 1)));
    }
}
