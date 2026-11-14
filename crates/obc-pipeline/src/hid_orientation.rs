//! Which way a camera faces, from an input report of its HID interface.
//!
//! Some cameras turn: the Logitech MX Brio flips down to the desk (its
//! Show Mode) and turns its picture over by itself. It says so on its HID
//! interface, in an input report listed as a
//! [`crate::quirks::StatusQuirk`] row ([`crate::quirks::OrientationReport`]):
//! [`parse`] reads a report into an [`Orientation`].
//!
//! **Finding the node** ([`hidraw_of_node`]). The `hidraw` node of the
//! camera is found from its video node through sysfs, never by its name:
//! the video node's USB device ([`crate::xu::usb_device_of_node`]), then
//! under its interfaces (`<device>/<device>:<config>.<n>/<hid>/hidraw/
//! hidrawN`) the HID device whose report descriptor declares the report.
//!
//! **Reading** ([`OrientationWatcher`]). A thread opens the node read-only
//! and non-blocking, waits in `poll()` for a report or its stop signal (an
//! eventfd), and hands each orientation that settled to a callback. The
//! camera sends the report on a change only, and its value can bounce
//! during the motion (01, 00, 01 within about 1.3 s): an orientation is
//! taken once it held for [`SETTLE`] with no other report ([`Debouncer`]).
//! The orientation is unknown until the camera's first report (the MX
//! Brio sends one when it is turned).
//!
//! **Passive only: no request ever goes to the device.** The reader only
//! reads the interrupt-IN reports the kernel already received; it issues
//! no ioctl on the node and writes nothing to it: no `HIDIOCGINPUT` or
//! `HIDIOCGFEATURE` (`GET_REPORT`), no `HIDIOCSFEATURE` or output report
//! (`SET_REPORT`), no `write()`. Each of those is a USB control request
//! the camera's firmware answers. Measured on an MX Brio (046d:0944): a
//! `HIDIOCGINPUT` for input report `0x55` (64-byte buffer) returned 0
//! bytes, the next three timed out (`ETIMEDOUT`), the fifth failed with
//! `EPROTO`, and the camera then stopped answering and dropped off the bus
//! ("device not responding to setup address"); in the daemon the same
//! request coincided with the Thunderbolt xHCI host controller dying ("HC
//! died"). The report descriptor is read from sysfs, the kernel's copy
//! taken at enumeration, never from the device.
//! `a_reader_sends_no_request_to_the_device` pins this.
//!
//! Dropping the watcher (or [`OrientationWatcher::stop`]) wakes the thread
//! and returns at once, without waiting for it. No notice is handed over
//! once the stop returned (a gate the thread takes around each callback);
//! the thread then ends by itself at its next wake-up, closing the node. When
//! the node goes away (the camera is unplugged) or reports an error, the
//! thread ends by itself after one [`OrientationNotice::Ended`].

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::quirks::OrientationReport;

/// How long an orientation must hold, with no other report, before it is
/// taken: longer than the bounce of the camera's motion.
pub const SETTLE: Duration = Duration::from_millis(1500);

/// The largest report read: longer reports are cut (the orientation byte
/// is near the start).
const REPORT_MAX: usize = 64;

/// Which way the camera faces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    /// Upright, facing ahead.
    Normal,
    /// Turned down to the desk (the MX Brio's Show Mode).
    DeskView,
}

/// The orientation an input report `report` says (the report id at byte
/// 0), for the camera's row `spec`; `None` for another report, or one too
/// short to hold the byte.
pub fn parse(report: &[u8], spec: &OrientationReport) -> Option<Orientation> {
    if report.first() != Some(&spec.report_id) {
        return None;
    }
    let byte = *report.get(spec.byte)?;
    Some(if byte == spec.flipped {
        Orientation::DeskView
    } else {
        Orientation::Normal
    })
}

/// Takes an orientation once it held for its settle time with no other
/// value in between; the first value is taken the same way.
#[derive(Debug, Clone)]
pub struct Debouncer {
    settle: Duration,
    stable: Option<Orientation>,
    pending: Option<(Orientation, Instant)>,
}

impl Debouncer {
    /// A debouncer with nothing taken yet.
    pub fn new(settle: Duration) -> Debouncer {
        Debouncer {
            settle,
            stable: None,
            pending: None,
        }
    }

    /// The orientation taken last; `None` before the first.
    pub fn stable(&self) -> Option<Orientation> {
        self.stable
    }

    /// When the pending orientation is due to be taken, if one is.
    pub fn deadline(&self) -> Option<Instant> {
        self.pending.map(|(_, since)| since + self.settle)
    }

    /// The camera said `o` at `now`. A value back at the one taken drops
    /// what was pending; a new one starts its settle time (the same one
    /// again keeps it). Returns the orientation taken now, if any
    /// ([`Self::poll`]).
    pub fn feed(&mut self, o: Orientation, now: Instant) -> Option<Orientation> {
        if self.stable == Some(o) {
            self.pending = None;
        } else if self.pending.is_none_or(|(p, _)| p != o) {
            self.pending = Some((o, now));
        }
        self.poll(now)
    }

    /// Takes the pending orientation when it held for the settle time by
    /// `now`, and returns it; `None` otherwise.
    pub fn poll(&mut self, now: Instant) -> Option<Orientation> {
        let (o, since) = self.pending?;
        if now.saturating_duration_since(since) < self.settle {
            return None;
        }
        self.pending = None;
        self.stable = Some(o);
        Some(o)
    }
}

/// Whether the HID report descriptor `descriptor` declares the report id
/// `id` (a global `Report ID` item, `0x85`). Long items are skipped; a
/// descriptor cut short ends the walk.
pub fn declares_report_id(descriptor: &[u8], id: u8) -> bool {
    let mut i = 0;
    while i < descriptor.len() {
        let prefix = descriptor[i];
        if prefix == 0xfe {
            // A long item: its data size follows.
            let Some(&size) = descriptor.get(i + 1) else {
                return false;
            };
            i += 3 + usize::from(size);
            continue;
        }
        let size = match prefix & 0x03 {
            3 => 4,
            n => usize::from(n),
        };
        if prefix & 0xfc == 0x84 && size == 1 && descriptor.get(i + 1) == Some(&id) {
            return true;
        }
        i += 1 + size;
    }
    false
}

/// The `hidraw` node of the USB device of the video node `node`, under
/// `sysfs_root`: the device found by the node's number
/// ([`crate::xu::usb_device_of_node`]), then among the HID devices of its
/// interfaces the first (by name) whose `report_descriptor` declares
/// `report_id` ([`declares_report_id`]); its node is
/// `/dev/<hidrawN>`. `NotFound` when the device has none.
pub fn hidraw_of_node(sysfs_root: &Path, node: &Path, report_id: u8) -> io::Result<PathBuf> {
    let usb = crate::xu::usb_device_of_node(sysfs_root, node)?;
    let name = usb.dir.file_name().unwrap_or_default().to_os_string();
    let mut prefix = name.clone();
    prefix.push(":");
    let interfaces = sorted_dir(&usb.dir)?
        .into_iter()
        .filter(|p| file_name_starts_with(p, &prefix));
    for interface in interfaces {
        for hid in sorted_dir(&interface).unwrap_or_default() {
            let Ok(descriptor) = fs::read(hid.join("report_descriptor")) else {
                continue;
            };
            if !declares_report_id(&descriptor, report_id) {
                continue;
            }
            if let Some(raw) = sorted_dir(&hid.join("hidraw"))
                .unwrap_or_default()
                .into_iter()
                .find(|p| file_name_starts_with(p, OsStr::new("hidraw")))
            {
                return Ok(Path::new("/dev").join(raw.file_name().unwrap_or_default()));
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "{}: no HID interface of USB device {} sends report {report_id:#04x}",
            node.display(),
            name.to_string_lossy()
        ),
    ))
}

/// The entries of `dir`, sorted by name.
fn sorted_dir(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    Ok(entries)
}

fn file_name_starts_with(path: &Path, prefix: &OsStr) -> bool {
    path.file_name()
        .is_some_and(|n| n.as_encoded_bytes().starts_with(prefix.as_encoded_bytes()))
}

/// What the watcher hands its callback.
#[derive(Debug)]
pub enum OrientationNotice {
    /// The camera's orientation settled at this value (the first one, or
    /// a change).
    Changed(Orientation),
    /// The watcher ended by itself, for this reason (the node went away or
    /// reported an error). Sent at most once, as the last notice, and
    /// never after a stop.
    Ended(io::Error),
}

/// Opens the `hidraw` node `path` for reading only, non-blocking.
/// `PermissionDenied` when the user may not read it. Never opened for
/// writing: nothing may be sent to the device (see the module docs).
pub fn open_hidraw(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
}

fn eventfd() -> io::Result<OwnedFd> {
    // SAFETY: plain syscall without pointers.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// A running orientation watcher ([`OrientationWatcher::start`]).
/// Dropping it stops the thread without waiting for it (see the module
/// docs).
#[derive(Debug)]
pub struct OrientationWatcher {
    wake: Arc<OwnedFd>,
    /// `true` once stopped: the thread hands over a notice only while it
    /// holds this lock and it is `false`.
    stopped: Arc<Mutex<bool>>,
    thread: Option<JoinHandle<()>>,
}

/// The callback, behind the stop gate: a notice is handed over only while
/// the watcher is not stopped, under the gate's lock, so none is once
/// [`OrientationWatcher::stop`] returned.
struct Gated<F> {
    stopped: Arc<Mutex<bool>>,
    on_notice: F,
}

impl<F: FnMut(OrientationNotice)> Gated<F> {
    fn send(&mut self, notice: OrientationNotice) {
        let stopped = self.stopped.lock().unwrap_or_else(|e| e.into_inner());
        if !*stopped {
            (self.on_notice)(notice);
        }
    }
}

impl OrientationWatcher {
    /// Opens the `hidraw` node `path` ([`open_hidraw`]) and starts the
    /// thread that reads its reports of `spec`, and calls `on_notice` (on
    /// the watcher thread) with each orientation that held for `settle`.
    /// `Err` when the node cannot be opened (`PermissionDenied`: no read
    /// access) or the thread not started.
    pub fn start(
        path: &Path,
        spec: OrientationReport,
        settle: Duration,
        on_notice: impl FnMut(OrientationNotice) + Send + 'static,
    ) -> io::Result<OrientationWatcher> {
        let file = open_hidraw(path)?;
        OrientationWatcher::spawn(file, spec, settle, on_notice)
    }

    /// Starts the watcher thread on the open descriptor `file`.
    fn spawn(
        file: fs::File,
        spec: OrientationReport,
        settle: Duration,
        on_notice: impl FnMut(OrientationNotice) + Send + 'static,
    ) -> io::Result<OrientationWatcher> {
        let wake = Arc::new(eventfd()?);
        let stopped = Arc::new(Mutex::new(false));
        let thread_wake = Arc::clone(&wake);
        let mut gated = Gated {
            stopped: Arc::clone(&stopped),
            on_notice,
        };
        let thread = std::thread::Builder::new()
            .name("obc-hid-orient".into())
            .spawn(move || {
                // The descriptor is the thread's: closed when `watch`
                // returns, a stop or not.
                let ended = watch(file, &thread_wake, spec, settle, &mut gated);
                if let Some(e) = ended {
                    gated.send(OrientationNotice::Ended(e));
                }
            })?;
        Ok(OrientationWatcher {
            wake,
            stopped,
            thread: Some(thread),
        })
    }

    /// Whether the thread has ended (by itself, after an
    /// [`OrientationNotice::Ended`]).
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }

    /// Stops the thread, without waiting for it to end; the same as
    /// dropping the watcher. No notice is sent after it returns (it waits
    /// at most for a callback in progress); the thread ends at its next
    /// wake-up and closes the node.
    pub fn stop(self) {}

    fn shutdown(&mut self) {
        // Detached: never joined (see the module docs).
        let Some(_thread) = self.thread.take() else {
            return;
        };
        *self.stopped.lock().unwrap_or_else(|e| e.into_inner()) = true;
        let one = 1u64.to_ne_bytes();
        // SAFETY: writes 8 bytes from a live buffer to the eventfd we own.
        let n = unsafe { libc::write(self.wake.as_raw_fd(), one.as_ptr().cast(), one.len()) };
        if n != one.len() as isize {
            // The gate keeps it silent all the same; it ends when its node
            // does.
            eprintln!(
                "could not wake the orientation watcher to stop: {}",
                io::Error::last_os_error()
            );
        }
    }
}

impl Drop for OrientationWatcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The watcher thread's loop: waits for a report on `file`, a stop on `wake`, or the debouncer's
/// deadline, and hands each orientation taken to `on_notice`. `None` when
/// stopped; the error that ended it otherwise. The descriptor is closed
/// when it returns.
fn watch<F: FnMut(OrientationNotice)>(
    mut file: fs::File,
    wake: &OwnedFd,
    spec: OrientationReport,
    settle: Duration,
    on_notice: &mut Gated<F>,
) -> Option<io::Error> {
    // Unknown until the camera's first report: never asked for its current
    // value, since a GET_REPORT hangs the MX Brio (see the module docs).
    let mut debouncer = Debouncer::new(settle);
    let mut buf = [0u8; REPORT_MAX];
    loop {
        let timeout = match debouncer.deadline() {
            None => -1,
            Some(at) => {
                let ms = at.saturating_duration_since(Instant::now()).as_millis();
                // Rounded up, so the wait never ends before the deadline.
                i32::try_from(ms + 1).unwrap_or(i32::MAX)
            }
        };
        let mut fds = [
            libc::pollfd {
                fd: file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `fds` is a valid array of two pollfd for the call; both
        // descriptors stay open (the file is this thread's, the eventfd is
        // Arc'ed).
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Some(e);
        }
        if fds[1].revents != 0 {
            return None;
        }
        let revents = fds[0].revents;
        if revents & libc::POLLNVAL != 0 {
            return Some(io::Error::other("the node's descriptor is not valid"));
        }
        if revents & libc::POLLIN != 0 {
            loop {
                match file.read(&mut buf) {
                    Ok(0) => return Some(io::Error::other("the node was closed")),
                    Ok(n) => {
                        if let Some(o) = parse(&buf[..n], &spec)
                            && let Some(taken) = debouncer.feed(o, Instant::now())
                        {
                            on_notice.send(OrientationNotice::Changed(taken));
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    // A vanished node answers ENODEV.
                    Err(e) => return Some(e),
                }
            }
        } else if revents & (libc::POLLERR | libc::POLLHUP) != 0 {
            return Some(io::Error::other("the node reported an error or hung up"));
        }
        if let Some(taken) = debouncer.poll(Instant::now()) {
            on_notice.send(OrientationNotice::Changed(taken));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use std::sync::mpsc;

    const BRIO: OrientationReport = OrientationReport {
        report_id: 0x55,
        byte: 5,
        flipped: 0x01,
    };
    const DESK: [u8; 7] = [0x55, 0x00, 0x00, 0xa4, 0x00, 0x01, 0x00];
    const UPRIGHT: [u8; 7] = [0x55, 0x00, 0x00, 0xa4, 0x00, 0x00, 0x00];

    #[test]
    fn the_brio_reports_parse() {
        assert_eq!(parse(&DESK, &BRIO), Some(Orientation::DeskView));
        assert_eq!(parse(&UPRIGHT, &BRIO), Some(Orientation::Normal));
        // Its other reports say nothing of it.
        let mut other = DESK;
        other[0] = 0x0a;
        assert_eq!(parse(&other, &BRIO), None);
        other[0] = 0x40;
        assert_eq!(parse(&other, &BRIO), None);
        // Too short to hold byte 5; empty.
        assert_eq!(parse(&DESK[..5], &BRIO), None);
        assert_eq!(parse(&[], &BRIO), None);
        // Longer is fine.
        let mut long = DESK.to_vec();
        long.extend([0xff; 8]);
        assert_eq!(parse(&long, &BRIO), Some(Orientation::DeskView));
    }

    #[test]
    fn an_orientation_is_taken_once_it_held_for_the_settle_time() {
        use Orientation::{DeskView, Normal};
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut d = Debouncer::new(SETTLE);
        // The first value, the same way.
        assert_eq!(d.feed(Normal, at(0)), None);
        assert_eq!(d.deadline(), Some(at(1500)));
        assert_eq!(d.poll(at(1499)), None);
        assert_eq!(d.poll(at(1500)), Some(Normal));
        assert_eq!(d.stable(), Some(Normal));
        assert_eq!(d.deadline(), None);
        // The motion bounces 1/0/1 within 1.3 s: no flapping, the last
        // value once it held.
        let mut taken = Vec::new();
        for (ms, o) in [(10_000, DeskView), (10_600, Normal), (11_300, DeskView)] {
            taken.extend(d.feed(o, at(ms)));
        }
        for ms in (11_300..13_000).step_by(100) {
            taken.extend(d.poll(at(ms)));
        }
        assert_eq!(taken, [DeskView]);
        assert_eq!(d.stable(), Some(DeskView));
        // A bounce away and back before it settles changes nothing.
        assert_eq!(d.feed(Normal, at(20_000)), None);
        assert_eq!(d.feed(DeskView, at(20_400)), None);
        assert_eq!(d.deadline(), None);
        assert_eq!(d.poll(at(30_000)), None);
        // The same value again keeps its start.
        assert_eq!(d.feed(Normal, at(40_000)), None);
        assert_eq!(d.feed(Normal, at(41_000)), None);
        assert_eq!(d.feed(Normal, at(41_500)), Some(Normal));
    }

    /// The MX Brio's report descriptor, as its sysfs lists it.
    const BRIO_DESCRIPTOR: &[u8] = &[
        0x06, 0x00, 0xff, 0x09, 0x01, 0xa1, 0x01, 0x85, 0x40, 0x15, 0x00, 0x25, 0x01, 0x09, 0x02,
        0x75, 0x01, 0x95, 0x01, 0x81, 0x06, 0x09, 0x03, 0x75, 0x01, 0x95, 0x01, 0x81, 0x06, 0x95,
        0x06, 0x81, 0x01, 0x85, 0x41, 0x15, 0x00, 0x26, 0xff, 0x00, 0x09, 0x01, 0x95, 0x02, 0x75,
        0x08, 0x91, 0x02, 0x85, 0x42, 0x09, 0x01, 0x81, 0x02, 0x85, 0x43, 0x09, 0x01, 0x81, 0x02,
        0x85, 0x44, 0x09, 0x01, 0x95, 0x04, 0x91, 0x02, 0x85, 0x45, 0x09, 0x01, 0x81, 0x02, 0x85,
        0x46, 0x95, 0x02, 0x09, 0x01, 0x81, 0x02, 0x85, 0x47, 0x09, 0x01, 0x81, 0x02, 0xc0, 0x06,
        0x01, 0xff, 0x09, 0x01, 0xa1, 0x01, 0x85, 0x0a, 0x15, 0x00, 0x26, 0xff, 0x00, 0x09, 0x01,
        0x95, 0x1f, 0x75, 0x08, 0x81, 0x02, 0xc0, 0x06, 0x02, 0xff, 0x09, 0x01, 0xa1, 0x01, 0x05,
        0x09, 0x19, 0x01, 0x29, 0x01, 0x15, 0x00, 0x25, 0x7f, 0x85, 0x01, 0x95, 0x01, 0x75, 0x08,
        0x81, 0x02, 0xc0, 0x06, 0xa1, 0xff, 0x09, 0x03, 0xa1, 0x01, 0x85, 0x55, 0x15, 0x00, 0x26,
        0xff, 0x00, 0x09, 0x03, 0x95, 0x06, 0x75, 0x08, 0x81, 0x02, 0xc0,
    ];

    #[test]
    fn report_ids_are_read_from_the_descriptor() {
        for id in [0x40, 0x41, 0x0a, 0x01, 0x55] {
            assert!(declares_report_id(BRIO_DESCRIPTOR, id), "{id:#04x}");
        }
        assert!(!declares_report_id(BRIO_DESCRIPTOR, 0x56));
        // 0xff appears as data (a Logical Maximum of 0x00ff, a Usage Page
        // of 0xff00), never as a Report ID.
        assert!(!declares_report_id(BRIO_DESCRIPTOR, 0xff));
        // A long item's data is skipped; a cut descriptor ends the walk.
        assert!(!declares_report_id(&[0xfe, 0x02, 0x00, 0x85, 0x55], 0x55));
        assert!(declares_report_id(
            &[0xfe, 0x02, 0x00, 0x85, 0x55, 0x85, 0x56],
            0x56
        ));
        assert!(!declares_report_id(&[0x85], 0x55));
        assert!(!declares_report_id(&[], 0x55));
    }

    /// The `major:minor` of a character device of this machine.
    fn char_number(path: &str) -> String {
        let rdev = fs::metadata(path).unwrap().rdev();
        format!("{}:{}", libc::major(rdev), libc::minor(rdev))
    }

    /// A fake sysfs: `dev/char/<major:minor of /dev/null>` is the video
    /// node `video7` of interface `2-2:1.0` of USB device `2-2`; interface
    /// `2-2:1.2` has HID device `0003:046D:0944.0001` with `hidraw3`
    /// (declaring `0x55` when `with_report`), and interface `2-2:1.1` a HID
    /// device whose descriptor declares only `0x40` (`hidraw2`). A decoy
    /// USB device `2-3` has a HID device declaring `0x55` (`hidraw9`).
    fn fake_sysfs(with_report: bool) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let usb = root.path().join("devices/pci0000:00/usb2/2-2");
        let class_dev = usb.join("2-2:1.0/video4linux/video7");
        fs::create_dir_all(&class_dev).unwrap();
        std::os::unix::fs::symlink("../../../2-2:1.0", class_dev.join("device")).unwrap();
        fs::write(usb.join("idVendor"), "046d\n").unwrap();
        fs::write(usb.join("idProduct"), "0944\n").unwrap();
        fs::write(usb.join("bConfigurationValue"), "1\n").unwrap();
        fs::write(usb.join("descriptors"), b"").unwrap();
        let hid = |dir: &Path, descriptor: &[u8], raw: &str| {
            fs::create_dir_all(dir.join("hidraw").join(raw)).unwrap();
            fs::write(dir.join("report_descriptor"), descriptor).unwrap();
        };
        hid(
            &usb.join("2-2:1.1/0003:046D:0944.0002"),
            &[0x85, 0x40],
            "hidraw2",
        );
        let brio: &[u8] = if with_report {
            BRIO_DESCRIPTOR
        } else {
            &[0x85, 0x0a]
        };
        hid(&usb.join("2-2:1.2/0003:046D:0944.0001"), brio, "hidraw3");
        hid(
            &root
                .path()
                .join("devices/pci0000:00/usb2/2-3/2-3:1.0/0003:1234:ABCD.0003"),
            BRIO_DESCRIPTOR,
            "hidraw9",
        );
        fs::create_dir_all(root.path().join("dev/char")).unwrap();
        std::os::unix::fs::symlink(
            "../../devices/pci0000:00/usb2/2-2/2-2:1.0/video4linux/video7",
            root.path().join("dev/char").join(char_number("/dev/null")),
        )
        .unwrap();
        root
    }

    #[test]
    fn the_hidraw_node_is_the_one_of_the_same_usb_device_declaring_the_report() {
        let root = fake_sysfs(true);
        let node = hidraw_of_node(root.path(), Path::new("/dev/null"), 0x55).unwrap();
        assert_eq!(node, Path::new("/dev/hidraw3"));
        // The report of the other interface: its node.
        let node = hidraw_of_node(root.path(), Path::new("/dev/null"), 0x40).unwrap();
        assert_eq!(node, Path::new("/dev/hidraw2"));
        // No interface of the device declares it (only the decoy does).
        let root = fake_sysfs(false);
        let e = hidraw_of_node(root.path(), Path::new("/dev/null"), 0x55).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(e.to_string().contains("0x55"), "{e}");
        assert!(e.to_string().contains("2-2"), "{e}");
        // A node sysfs does not know.
        let e = hidraw_of_node(root.path(), Path::new("/dev/zero"), 0x55).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
    }

    /// The reader never sends a request to the device (see the module
    /// docs): outside the tests, the code issues no ioctl at all, never
    /// opens a node for writing, and its one `write()` goes to the stop
    /// eventfd.
    #[test]
    fn a_reader_sends_no_request_to_the_device() {
        let source = include_str!("hid_orientation.rs");
        let code: Vec<&str> = source
            .split("#[cfg(test)]")
            .next()
            .unwrap()
            .lines()
            .map(str::trim_start)
            .filter(|l| !l.starts_with("//"))
            .collect();
        for word in ["ioctl", "HIDIOC", ".write(true)", "Write", "write_all"] {
            let hits: Vec<&&str> = code.iter().filter(|l| l.contains(word)).collect();
            assert!(hits.is_empty(), "{word}: {hits:?}");
        }
        let writes: Vec<&&str> = code.iter().filter(|l| l.contains("write(")).collect();
        assert_eq!(writes.len(), 1, "{writes:?}");
        assert!(writes[0].contains("self.wake"), "{writes:?}");
    }

    /// A pair of connected sequenced-packet sockets: each write is one
    /// read on the other end, as each report is on a `hidraw` node.
    fn packet_pair() -> (fs::File, fs::File) {
        let mut fds = [0; 2];
        // SAFETY: `fds` is a valid array of two ints for the call.
        let rc = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
                fds.as_mut_ptr(),
            )
        };
        assert_eq!(rc, 0);
        // SAFETY: both are fresh descriptors nothing else owns.
        unsafe { (fs::File::from_raw_fd(fds[0]), fs::File::from_raw_fd(fds[1])) }
    }

    fn send(device: &mut fs::File, report: &[u8]) {
        use std::io::Write;
        assert_eq!(device.write(report).unwrap(), report.len());
    }

    #[test]
    fn the_watcher_hands_over_settled_orientations_and_ends_when_the_node_goes() {
        let (reader, mut device) = packet_pair();
        let (tx, rx) = mpsc::channel();
        let settle = Duration::from_millis(100);
        let w = OrientationWatcher::spawn(reader, BRIO, settle, move |n| {
            tx.send(n).unwrap();
        })
        .unwrap();
        // Nothing is asked of the device: unknown until a report.
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
        // Other reports, a bounce, then desk view held.
        let mut other = DESK;
        other[0] = 0x0a;
        send(&mut device, &other);
        send(&mut device, &DESK);
        send(&mut device, &UPRIGHT);
        send(&mut device, &DESK);
        let n = rx.recv_timeout(Duration::from_secs(5)).expect("a notice");
        assert!(
            matches!(n, OrientationNotice::Changed(Orientation::DeskView)),
            "{n:?}"
        );
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
        send(&mut device, &UPRIGHT);
        let n = rx.recv_timeout(Duration::from_secs(5)).expect("a notice");
        assert!(
            matches!(n, OrientationNotice::Changed(Orientation::Normal)),
            "{n:?}"
        );
        // The device goes: one Ended, then the thread is gone.
        drop(device);
        let n = rx.recv_timeout(Duration::from_secs(5)).expect("a notice");
        assert!(matches!(n, OrientationNotice::Ended(_)), "{n:?}");
        let t = Instant::now();
        while !w.is_finished() {
            assert!(t.elapsed() < Duration::from_secs(5), "still running");
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(w);
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn a_running_watcher_stops_at_once_without_a_notice() {
        let (reader, mut device) = packet_pair();
        let (tx, rx) = mpsc::channel();
        let w = OrientationWatcher::spawn(reader, BRIO, SETTLE, move |n| {
            tx.send(format!("{n:?}")).unwrap();
        })
        .unwrap();
        // A pending value, not settled yet: the stop does not wait for it.
        send(&mut device, &DESK);
        std::thread::sleep(Duration::from_millis(20));
        assert!(!w.is_finished());
        let t = Instant::now();
        w.stop();
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_node_that_cannot_be_opened_is_an_error() {
        let e = OrientationWatcher::start(Path::new("/nonexistent/hidraw0"), BRIO, SETTLE, |_| {})
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        // A node without read access.
        let dir = tempfile::tempdir().unwrap();
        let node = dir.path().join("hidraw0");
        fs::write(&node, b"").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&node, fs::Permissions::from_mode(0o200)).unwrap();
        // Root reads anything: only checked as a user.
        // SAFETY: plain syscall.
        if unsafe { libc::geteuid() } != 0 {
            let e = OrientationWatcher::start(&node, BRIO, SETTLE, |_| {}).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        }
    }
}
