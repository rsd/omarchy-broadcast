//! The lifecycle state machine against the fake backend, with explicit
//! instants: every transition of the ADR-0011 diagram, the debounce and the
//! grace, persistence, camera selection, hotplug, reloads and the writer.

use std::fs;
use std::time::{Duration, Instant};

use obc_core::config::RuntimeState;
use obc_ipc::{IpcError, State, Stats};
use tempfile::TempDir;
use tokio::sync::mpsc::UnboundedReceiver;

use super::*;
use crate::backend::fake::{FakeBackend, Hook, World};
use crate::camera::tests::cam;
use crate::log::Level;

fn alpha() -> Camera {
    cam("usb-a", "Alpha Cam", Some("1111:0001"), "/dev/video0")
}

fn beta() -> Camera {
    cam("usb-b", "Beta Cam", Some("2222:0002"), "/dev/video2")
}

/// A controller over a fake world, its files in a temporary directory and
/// time counted in milliseconds from `t0`.
struct H {
    ctrl: Controller<FakeBackend>,
    fake: FakeBackend,
    notes: UnboundedReceiver<Note>,
    paths: Paths,
    t0: Instant,
    _dir: TempDir,
}

impl H {
    fn new(config: &str, setup: impl FnOnce(&mut World)) -> H {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config.toml"),
            state: dir.path().join("state").join("state.toml"),
        };
        if !config.is_empty() {
            fs::write(&paths.config, config).unwrap();
        }
        let fake = FakeBackend::default();
        {
            let mut w = fake.world();
            w.cameras = vec![alpha(), beta()];
            setup(&mut w);
        }
        let t0 = Instant::now();
        let (ctrl, notes) = Self::controller(&fake, &paths, t0);
        let mut h = H {
            ctrl,
            fake,
            notes,
            paths,
            t0,
            _dir: dir,
        };
        // The start-up changes (cameras found) are not under test here.
        h.drain();
        h
    }

    fn controller(
        fake: &FakeBackend,
        paths: &Paths,
        t0: Instant,
    ) -> (Controller<FakeBackend>, UnboundedReceiver<Note>) {
        let (published, notes) = Published::new(preliminary_snapshot(paths));
        let ctrl = Controller::new(fake.clone(), paths.clone(), published, None, t0);
        (ctrl, notes)
    }

    /// The daemon restarted: a new controller (and media world) on the
    /// same files.
    fn restart(&mut self) {
        let fake = FakeBackend::default();
        fake.world().cameras = self.fake.world().cameras.clone();
        self.ctrl.shutdown();
        let (ctrl, notes) = Self::controller(&fake, &self.paths, self.t0);
        self.ctrl = ctrl;
        self.notes = notes;
        self.fake = fake;
    }

    fn at(&self, ms: u64) -> Instant {
        self.t0 + Duration::from_millis(ms)
    }

    fn tick(&mut self, ms: u64) {
        let now = self.at(ms);
        self.ctrl.tick(now);
    }

    fn consumers(&mut self, n: u32, ms: u64) {
        let now = self.at(ms);
        assert!(self.ctrl.handle(Command::Consumers(n), now));
    }

    fn rescan(&mut self, ms: u64) {
        let now = self.at(ms);
        assert!(self.ctrl.handle(Command::Rescan, now));
    }

    fn call(&mut self, make: impl FnOnce(Reply) -> Command, ms: u64) -> Result<(), IpcError> {
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let now = self.at(ms);
        assert!(self.ctrl.handle(make(tx), now));
        rx.try_recv()
            .expect("the controller replies before returning")
    }

    fn reload(&mut self, config: &str, ms: u64) -> Result<(), IpcError> {
        fs::write(&self.paths.config, config).unwrap();
        self.call(|r| Command::ReloadConfig(Some(r)), ms)
    }

    fn snap(&self) -> Snapshot {
        self.ctrl.snapshot()
    }

    fn state(&self) -> State {
        self.snap().state
    }

    fn world(&self) -> std::sync::MutexGuard<'_, World> {
        self.fake.world()
    }

    /// Notes since the last call: property changes and stats.
    fn drain(&mut self) -> (Vec<Change>, Vec<Stats>) {
        let (mut changes, mut stats) = (Vec::new(), Vec::new());
        while let Ok(n) = self.notes.try_recv() {
            match n {
                Note::Changed(c) => changes.extend(c),
                Note::Stats(s) => stats.push(s),
            }
        }
        (changes, stats)
    }

    fn states(&mut self) -> Vec<State> {
        self.drain()
            .0
            .into_iter()
            .filter_map(|c| match c {
                Change::State(s) => Some(s),
                _ => None,
            })
            .collect()
    }

    /// A consumer from `ms` on, the camera started after the debounce and
    /// its first frame seen: `active` at `ms + 300`.
    fn activate(&mut self, ms: u64) {
        self.consumers(1, ms);
        self.tick(ms + 300);
        assert_eq!(self.state(), State::Active, "{:?}", self.snap());
    }

    fn persisted(&self) -> RuntimeState {
        let (s, warn) = RuntimeState::load(&self.paths.state);
        assert_eq!(warn, None);
        s
    }
}

fn ids(s: &Snapshot) -> Vec<&str> {
    s.cameras.iter().map(|c| c.id.as_str()).collect()
}

#[test]
fn starts_idle_with_the_writer_and_no_session() {
    let mut h = H::new("[camera]\npreference = [\"beta\"]\n", |_| {});
    let s = h.snap();
    assert_eq!(s.state, State::Idle);
    assert_eq!(ids(&s), ["usb-b", "usb-a"], "preference first");
    assert_eq!(s.active_camera, "usb-b");
    assert_eq!(s.output_mode, "NV12 1920x1080@30");
    assert_eq!(s.effect, "none");
    assert_eq!((s.enabled, s.consumers, s.error.as_str()), (true, 0, ""));
    assert_eq!(s.interface_revision, obc_ipc::INTERFACE_REVISION);
    assert_eq!(s.cameras[0].usb, "2222:0002");
    {
        let w = h.world();
        assert_eq!(w.writer_starts, 1);
        assert_eq!(w.writer.as_ref().unwrap().0, PathBuf::from("/dev/video11"));
        assert!(w.session.is_none());
    }
    // Idle with nothing scheduled sleeps long.
    assert!(h.ctrl.next_wakeup(h.at(0)) >= Duration::from_secs(30));
    h.tick(60_000);
    assert_eq!(h.state(), State::Idle);
    assert_eq!(h.world().session_starts, 0);
}

#[test]
fn disabled_in_the_state_file_starts_off_with_the_writer_running() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.toml");
    RuntimeState {
        enabled: false,
        ..RuntimeState::default()
    }
    .save(&state)
    .unwrap();
    let paths = Paths {
        config: dir.path().join("none.toml"),
        state,
    };
    let fake = FakeBackend::default();
    fake.world().cameras = vec![alpha()];
    let (published, _notes) = Published::new(preliminary_snapshot(&paths));
    assert_eq!(published.snapshot().state, State::Off);
    let mut ctrl = Controller::new(fake.clone(), paths, published, None, Instant::now());
    assert_eq!(ctrl.snapshot().state, State::Off);
    assert!(!ctrl.snapshot().enabled);
    assert_eq!(
        fake.world().writer_starts,
        1,
        "the placeholder flows while off"
    );
    // A consumer does not start the camera while off.
    let t = Instant::now();
    ctrl.handle(Command::Consumers(1), t);
    ctrl.tick(t + Duration::from_secs(2));
    assert_eq!(ctrl.snapshot().state, State::Off);
    assert_eq!(ctrl.snapshot().consumers, 1);
    assert_eq!(fake.world().session_starts, 0);
}

#[test]
fn a_short_open_never_starts_the_camera() {
    let mut h = H::new("", |_| {});
    h.consumers(1, 0);
    assert_eq!(h.ctrl.next_wakeup(h.at(0)), Duration::from_millis(300));
    h.tick(100);
    assert_eq!(h.state(), State::Idle);
    h.consumers(0, 100);
    for ms in [300, 400, 1000, 10_000] {
        h.tick(ms);
    }
    assert_eq!(h.state(), State::Idle);
    assert_eq!(h.world().session_starts, 0);
    // Opens shorter than the debounce, back to back, never add up.
    for k in 0..10 {
        h.consumers(1, 20_000 + k * 200);
        h.tick(20_000 + k * 200 + 150);
        h.consumers(0, 20_000 + k * 200 + 150);
    }
    h.tick(30_000);
    assert_eq!(h.world().session_starts, 0);
    assert_eq!(h.states(), Vec::<State>::new());
}

#[test]
fn a_held_open_starts_then_goes_active_on_the_first_frame() {
    let mut h = H::new("", |w| w.frames = false);
    h.consumers(1, 0);
    h.tick(299);
    assert_eq!(h.state(), State::Idle);
    h.tick(300);
    assert_eq!(h.state(), State::Starting);
    {
        let w = h.world();
        assert_eq!(w.session_starts, 1);
        assert_eq!(w.ingests, ["usb-a"]);
    }
    assert_eq!(h.snap().active_camera, "usb-a");
    // Polled quickly while starting.
    assert!(h.ctrl.next_wakeup(h.at(300)) <= Duration::from_millis(50));
    h.tick(350);
    assert_eq!(h.state(), State::Starting);
    h.world().frames = true;
    h.tick(400);
    assert_eq!(h.state(), State::Active);
    let (changes, _) = h.drain();
    let states: Vec<_> = changes
        .iter()
        .filter_map(|c| match c {
            Change::State(s) => Some(*s),
            _ => None,
        })
        .collect();
    assert_eq!(states, [State::Starting, State::Active]);
    assert_eq!(changes.first(), Some(&Change::Consumers(1)));
}

#[test]
fn the_grace_period_keeps_the_session_for_a_returning_consumer() {
    let mut h = H::new("", |_| {});
    h.activate(0);
    h.consumers(0, 1000);
    assert_eq!(h.snap().consumers, 0);
    assert_eq!(h.ctrl.next_wakeup(h.at(1000)), Duration::from_millis(250));
    h.tick(5999);
    assert_eq!(h.state(), State::Active, "still in the grace period");
    h.consumers(1, 5999);
    h.consumers(0, 6500);
    h.tick(11_499);
    assert_eq!(h.state(), State::Active);
    assert_eq!(
        h.world().session_starts,
        1,
        "the returning consumer found it running"
    );
    h.tick(11_500);
    assert_eq!(h.state(), State::Idle);
    let w = h.world();
    assert!(w.session.is_none());
    assert_eq!((w.session_starts, w.session_stops), (1, 1));
    assert!(w.writer.is_some(), "the writer never stops");
    assert_eq!(w.writer_starts, 1);
}

#[test]
fn a_zero_grace_stops_at_once() {
    let mut h = H::new(
        "[lifecycle]\nidle_grace_s = 0.0\nstart_debounce_ms = 0\n",
        |_| {},
    );
    h.consumers(1, 0);
    assert_eq!(h.state(), State::Active, "no debounce: started on the open");
    h.consumers(0, 10);
    assert_eq!(h.state(), State::Idle);
}

#[test]
fn set_enabled_false_while_active_stops_the_session_and_keeps_the_writer() {
    let mut h = H::new("", |_| {});
    h.activate(0);
    h.drain();
    h.call(|r| Command::SetEnabled(false, r), 1000).unwrap();
    assert_eq!(h.state(), State::Off);
    assert!(!h.snap().enabled);
    {
        let w = h.world();
        assert!(w.session.is_none(), "camera and CUDA released");
        assert!(w.writer.is_some(), "the placeholder keeps flowing");
        assert_eq!(w.writer_starts, 1);
    }
    assert!(!h.persisted().enabled);
    let (changes, _) = h.drain();
    assert_eq!(changes, [Change::Enabled(false), Change::State(State::Off)]);
    // The consumer is still there: nothing restarts while off.
    h.tick(20_000);
    assert_eq!(h.state(), State::Off);
    assert_eq!(h.world().session_starts, 1);
    // Setting the same value again is not an error.
    h.call(|r| Command::SetEnabled(false, r), 20_000).unwrap();

    // Persisted: a restarted daemon is off.
    h.restart();
    assert_eq!(h.state(), State::Off);
    assert!(!h.snap().enabled);

    // SetEnabled(true) with a consumer present: straight to starting.
    h.consumers(1, 30_000);
    h.tick(31_000);
    assert_eq!(h.state(), State::Off);
    h.world().frames = false;
    h.call(|r| Command::SetEnabled(true, r), 31_000).unwrap();
    assert_eq!(h.state(), State::Starting);
    assert!(h.persisted().enabled);
    h.world().frames = true;
    h.tick(31_100);
    assert_eq!(h.state(), State::Active);
}

#[test]
fn set_enabled_true_without_a_consumer_is_idle() {
    let mut h = H::new("", |_| {});
    h.call(|r| Command::SetEnabled(false, r), 0).unwrap();
    assert_eq!(h.state(), State::Off);
    h.call(|r| Command::SetEnabled(true, r), 10).unwrap();
    assert_eq!(h.state(), State::Idle);
    assert_eq!(h.world().session_starts, 0);
    assert_eq!(h.states(), [State::Off, State::Idle]);
}

#[test]
fn a_state_file_that_cannot_be_written_fails_but_applies() {
    let mut h = H::new("", |_| {});
    // The state path's parent is a file: the save fails.
    fs::create_dir_all(h.paths.state.parent().unwrap().parent().unwrap()).unwrap();
    fs::write(h.paths.state.parent().unwrap(), b"not a dir").unwrap();
    match h.call(|r| Command::SetEnabled(false, r), 0) {
        Err(IpcError::Failed(m)) => assert!(m.contains("will not survive a restart"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(h.state(), State::Off);
}

#[test]
fn the_selection_persists_and_applies_at_the_next_start() {
    let mut h = H::new("", |_| {});
    assert_eq!(h.snap().active_camera, "usb-a");
    h.call(|r| Command::SelectCamera("beta".into(), r), 0)
        .unwrap();
    assert_eq!(h.snap().active_camera, "usb-b");
    assert_eq!(h.state(), State::Idle);
    assert_eq!(h.world().session_starts, 0, "idle: the selection only");
    assert_eq!(h.persisted().camera.as_deref(), Some("usb-b"));
    // Every selector form.
    for (sel, want) in [
        ("1111:0001", "usb-a"),
        ("/dev/video2", "usb-b"),
        ("usb-a", "usb-a"),
        ("BETA CAM", "usb-b"),
    ] {
        h.call(|r| Command::SelectCamera(sel.into(), r), 0).unwrap();
        assert_eq!(h.snap().active_camera, want, "{sel}");
    }
    match h.call(|r| Command::SelectCamera("gamma".into(), r), 0) {
        Err(IpcError::NoSuchCamera(m)) => assert!(m.contains("\"gamma\""), "{m}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        h.snap().active_camera,
        "usb-b",
        "unchanged by a failed select"
    );

    h.restart();
    assert_eq!(
        h.snap().active_camera,
        "usb-b",
        "the selection survives a restart"
    );
    h.activate(0);
    assert_eq!(h.world().ingests, ["usb-b"]);

    // A selected camera that is absent: the preference order applies.
    h.restart();
    h.fake.world().cameras = vec![alpha()];
    h.rescan(0);
    assert_eq!(h.snap().active_camera, "usb-a");
    assert_eq!(h.persisted().camera.as_deref(), Some("usb-b"));
}

#[test]
fn select_and_next_switch_a_live_session_without_leaving_active() {
    let mut h = H::new("", |_| {});
    h.activate(0);
    h.drain();
    h.call(|r| Command::SelectCamera("usb-b".into(), r), 1000)
        .unwrap();
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.snap().active_camera, "usb-b");
    h.call(Command::NextCamera, 2000).unwrap();
    assert_eq!(h.snap().active_camera, "usb-a", "wraps around");
    let w = h.world();
    assert_eq!(w.ingests, ["usb-a", "usb-b", "usb-a"]);
    assert_eq!(w.session_starts, 1, "the output is kept");
    drop(w);
    let (changes, _) = h.drain();
    assert!(
        changes.iter().all(|c| !matches!(c, Change::State(_))),
        "{changes:?}"
    );
    assert_eq!(h.persisted().camera.as_deref(), Some("usb-a"));
    // Selecting the streaming camera again changes nothing.
    h.call(|r| Command::SelectCamera("alpha".into(), r), 3000)
        .unwrap();
    assert_eq!(h.world().ingests.len(), 3);
}

#[test]
fn a_failed_live_switch_falls_back_and_reports_failed() {
    let mut h = H::new("", |w| w.fail_ingest = vec!["usb-b".into()]);
    h.activate(0);
    match h.call(|r| Command::SelectCamera("usb-b".into(), r), 1000) {
        Err(IpcError::Failed(m)) => assert!(m.contains("Beta Cam"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(h.world().ingests, ["usb-a", "usb-b", "usb-a"]);
    assert_eq!(h.snap().active_camera, "usb-a");
    assert_eq!(h.state(), State::Active);
}

#[test]
fn next_camera_wraps_when_idle_and_needs_a_camera() {
    let mut h = H::new("", |_| {});
    h.call(Command::NextCamera, 0).unwrap();
    assert_eq!(h.snap().active_camera, "usb-b");
    h.call(Command::NextCamera, 0).unwrap();
    assert_eq!(h.snap().active_camera, "usb-a");
    assert_eq!(h.world().session_starts, 0);
    // A single camera: selected again, not an error.
    h.fake.world().cameras = vec![beta()];
    h.rescan(0);
    h.call(Command::NextCamera, 0).unwrap();
    h.call(Command::NextCamera, 0).unwrap();
    assert_eq!(h.snap().active_camera, "usb-b");
    h.fake.world().cameras.clear();
    h.rescan(0);
    assert_eq!(h.snap().active_camera, "");
    match h.call(Command::NextCamera, 0) {
        Err(IpcError::Unavailable(m)) => assert!(m.contains("no camera"), "{m}"),
        other => panic!("{other:?}"),
    }
    match h.call(|r| Command::SelectCamera("usb-b".into(), r), 0) {
        Err(IpcError::NoSuchCamera(m)) => assert!(m.contains("cameras present: none"), "{m}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_failed_camera_falls_back_then_errors_then_recovers_when_one_appears() {
    let mut h = H::new("", |_| {});
    h.activate(0);
    h.world().failure = Some(Failure::Camera("Alpha: streaming error".into()));
    h.tick(1000);
    assert_eq!(h.state(), State::Active, "active -> active on a fallback");
    assert_eq!(h.snap().active_camera, "usb-b");
    assert_eq!(h.world().ingests, ["usb-a", "usb-b"]);

    // Alpha is unplugged; then Beta fails with no camera left.
    h.fake.world().cameras = vec![beta()];
    h.rescan(2000);
    assert_eq!(h.state(), State::Active);
    h.drain();
    h.world().failure = Some(Failure::Camera("Beta: streaming error".into()));
    h.tick(3000);
    assert_eq!(h.state(), State::Error);
    let e = h.snap().error;
    assert!(
        e.contains("Beta: streaming error") && e.contains("no other camera"),
        "{e}"
    );
    {
        let w = h.world();
        assert!(
            w.session.is_none(),
            "camera closed and CUDA released in error"
        );
        assert!(w.writer.is_some(), "placeholders keep flowing");
    }
    assert_eq!(h.states(), [State::Error]);

    // A camera appears while the consumer still holds the loopback.
    h.fake.world().cameras = vec![alpha(), beta()];
    h.rescan(4000);
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.snap().error, "", "cleared by the successful start");
    let w = h.world();
    assert_eq!(w.session_starts, 2);
    assert_eq!(w.ingests.last().map(String::as_str), Some("usb-a"));
}

#[test]
fn a_vanished_camera_falls_back_to_the_next() {
    let mut h = H::new("", |_| {});
    h.activate(0);
    h.fake.world().cameras = vec![beta()];
    h.rescan(1000);
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.snap().active_camera, "usb-b");
    assert_eq!(h.world().ingests, ["usb-a", "usb-b"]);
    assert_eq!(h.snap().cameras.len(), 1);
}

#[test]
fn a_camera_that_appears_does_not_preempt_the_streaming_one() {
    let mut h = H::new("[camera]\npreference = [\"alpha\"]\n", |w| {
        w.cameras = vec![beta()]
    });
    h.activate(0);
    assert_eq!(h.snap().active_camera, "usb-b");
    h.fake.world().cameras = vec![alpha(), beta()];
    h.rescan(1000);
    h.tick(2000);
    assert_eq!(h.snap().active_camera, "usb-b");
    assert_eq!(ids(&h.snap()), ["usb-a", "usb-b"]);
    assert_eq!(h.world().ingests, ["usb-b"]);
    // The next start uses the preferred camera again.
    h.consumers(0, 3000);
    h.tick(8000);
    assert_eq!(h.state(), State::Idle);
    assert_eq!(h.snap().active_camera, "usb-a");
}

#[test]
fn no_camera_is_an_error_until_the_consumer_leaves() {
    let mut h = H::new("", |w| w.cameras.clear());
    assert_eq!(h.state(), State::Idle);
    assert_eq!(h.snap().active_camera, "");
    h.consumers(1, 0);
    h.tick(300);
    assert_eq!(h.state(), State::Error);
    assert_eq!(h.snap().error, "no camera is available");
    assert_eq!(h.world().session_starts, 0);
    h.consumers(0, 1000);
    assert_eq!(h.state(), State::Idle, "error -> idle without a consumer");
}

#[test]
fn a_failed_start_is_an_error_and_a_method_call_retries() {
    let mut h = H::new("", |w| {
        w.fail_start = Some("CUDA initialisation failed".into())
    });
    h.activate_expecting_error();
    assert_eq!(h.snap().error, "CUDA initialisation failed");
    h.tick(5000);
    assert_eq!(h.world().session_starts, 0, "no retry by itself");
    h.world().fail_start = None;
    // Any control call leaves error; the consumer is present: starting.
    h.call(|r| Command::SetEnabled(true, r), 6000).unwrap();
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.snap().error, "");
}

impl H {
    fn activate_expecting_error(&mut self) {
        self.consumers(1, 0);
        self.tick(300);
        assert_eq!(self.state(), State::Error, "{:?}", self.snap());
    }
}

#[test]
fn an_ingest_that_fails_to_start_falls_through_to_the_next_camera() {
    let mut h = H::new("", |w| w.fail_ingest = vec!["usb-a".into()]);
    h.activate(0);
    assert_eq!(h.snap().active_camera, "usb-b");
    assert_eq!(h.world().ingests, ["usb-a", "usb-b"]);
    // Every camera failing: error with the last reason.
    let mut h = H::new("", |w| w.fail_ingest = vec!["usb-a".into(), "usb-b".into()]);
    h.activate_expecting_error();
    assert!(h.snap().error.contains("Beta Cam"), "{}", h.snap().error);
    assert!(h.world().session.is_none());
}

#[test]
fn a_camera_without_frames_times_out_and_the_next_takes_over() {
    let mut h = H::new("", |w| w.frames = false);
    h.consumers(1, 0);
    h.tick(300);
    assert_eq!(h.state(), State::Starting);
    h.tick(300 + 9_999);
    assert_eq!(h.world().ingests, ["usb-a"]);
    h.tick(300 + 10_000);
    assert_eq!(h.world().ingests, ["usb-a", "usb-b"]);
    assert_eq!(h.state(), State::Starting);
    h.world().frames = true;
    h.tick(10_400);
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.snap().active_camera, "usb-b");
}

#[test]
fn an_output_failure_ends_the_session_in_error() {
    let mut h = H::new("", |_| {});
    h.activate(0);
    h.world().failure = Some(Failure::Output("cudaconvertscale: internal error".into()));
    h.tick(1000);
    assert_eq!(h.state(), State::Error);
    assert!(h.world().session.is_none());
    assert_eq!(
        h.world().ingests,
        ["usb-a"],
        "no camera switch for an output failure"
    );
    // The consumer leaves: idle; it returns: a new session.
    h.consumers(0, 2000);
    assert_eq!(h.state(), State::Idle);
    h.activate(3000);
    assert_eq!(h.world().session_starts, 2);
}

#[test]
fn an_invalid_reload_keeps_the_old_config_and_sets_error() {
    let mut h = H::new("[camera]\npreference = [\"beta\"]\n", |_| {});
    h.activate(0);
    h.drain();
    let res = h.reload("[camera]\nmdoe = \"1280x720@30\"\n", 1000);
    let msg = match res {
        Err(IpcError::InvalidConfig(m)) => m,
        other => panic!("{other:?}"),
    };
    assert!(msg.contains("mdoe") && msg.contains("config.toml"), "{msg}");
    let s = h.snap();
    assert_eq!(s.error, msg);
    assert_eq!(
        s.state,
        State::Active,
        "an invalid config is not the error state"
    );
    assert_eq!(ids(&s), ["usb-b", "usb-a"], "the old preference stays");
    assert_eq!(h.ctrl.config().camera_preference().len(), 1);
    assert_eq!(h.drain().0, [Change::Error(msg.clone())]);

    // SIGHUP (no reply) with an unreadable value: the same.
    fs::write(&h.paths.config, "[lifecycle]\nidle_grace_s = -1\n").unwrap();
    h.ctrl.handle(Command::ReloadConfig(None), h.at(1500));
    assert!(
        h.snap().error.contains("idle_grace_s"),
        "{}",
        h.snap().error
    );
    assert_eq!(h.ctrl.config().idle_grace(), Duration::from_secs(5));

    // A valid file clears Error and applies.
    h.reload("[camera]\npreference = [\"alpha\"]\n", 2000)
        .unwrap();
    let s = h.snap();
    assert_eq!(s.error, "");
    assert_eq!(ids(&s), ["usb-a", "usb-b"]);
    assert_eq!(
        s.active_camera, "usb-b",
        "the streaming camera is not preempted"
    );
    // A missing file means the defaults.
    fs::remove_file(&h.paths.config).unwrap();
    h.call(|r| Command::ReloadConfig(Some(r)), 3000).unwrap();
    assert_eq!(h.ctrl.config(), &obc_core::config::Config::default());
}

#[test]
fn an_invalid_config_at_start_runs_on_the_defaults_with_error_set() {
    let h = H::new("version = 2\n", |_| {});
    let s = h.snap();
    assert_eq!(s.state, State::Idle);
    assert!(
        s.error.contains("version") && s.error.contains("running on the default configuration"),
        "{}",
        s.error
    );
    assert_eq!(h.ctrl.config(), &obc_core::config::Config::default());
}

#[test]
fn a_reload_applies_grace_debounce_and_session_settings() {
    let mut h = H::new("", |_| {});
    h.reload(
        "[lifecycle]\nidle_grace_s = 1.0\nstart_debounce_ms = 50\n\
         [output]\nhold_timeout_s = 1.5\ninput_gap_ms = 200\naspect = \"letterbox\"\n\
         [camera]\nfixed_framerate = true\n",
        0,
    )
    .unwrap();
    h.consumers(1, 1000);
    h.tick(1050);
    assert_eq!(h.state(), State::Active);
    {
        let w = h.world();
        let (p, _) = w.session.as_ref().unwrap();
        assert_eq!(p.hold_timeout, Duration::from_millis(1500));
        assert_eq!(p.input_gap, Duration::from_millis(200));
        assert_eq!(p.aspect, obc_core::AspectPolicy::Letterbox);
        assert!(p.fixed_framerate);
    }
    h.consumers(0, 2000);
    h.tick(2999);
    assert_eq!(h.state(), State::Active);
    h.tick(3000);
    assert_eq!(h.state(), State::Idle);
}

#[test]
fn a_camera_mode_change_on_reload_reopens_the_streaming_camera() {
    let mut h = H::new("", |w| w.follow_request = true);
    h.activate(0);
    h.reload("[camera]\nmode = \"1280x720@30\"\n", 1000)
        .unwrap();
    assert_eq!(h.world().ingests, ["usb-a", "usb-a"]);
    assert_eq!(h.state(), State::Active);
    // An unchanged mode does not.
    h.reload("[camera]\nmode = \"1280x720@30\"\n", 2000)
        .unwrap();
    assert_eq!(h.world().ingests.len(), 2);
}

#[test]
fn an_output_mode_change_waits_for_the_last_consumer() {
    let mut h = H::new("", |_| {});
    h.activate(0);
    h.drain();
    h.reload("[output]\nmode = \"1280x720@30\"\n", 1000)
        .unwrap();
    assert_eq!(
        h.snap().output_mode,
        "NV12 1920x1080@30",
        "still what the device carries"
    );
    assert_eq!(h.world().writer_starts, 1);
    h.tick(3000);
    assert_eq!(h.world().writer_starts, 1);
    h.consumers(0, 4000);
    h.tick(8999);
    assert_eq!(h.world().writer_starts, 1, "not during the grace period");
    h.tick(9000);
    assert_eq!(h.state(), State::Idle);
    {
        let w = h.world();
        assert_eq!(w.writer_starts, 2);
        assert_eq!(w.writer.as_ref().unwrap().1.width, 1280);
    }
    assert_eq!(h.snap().output_mode, "NV12 1280x720@30");
    let (changes, _) = h.drain();
    assert!(
        changes.contains(&Change::OutputMode("NV12 1280x720@30".into())),
        "{changes:?}"
    );
    // The next session uses the new contract (the fake checks it).
    h.activate(10_000);

    // With nobody holding the device the change applies at once.
    let mut h = H::new("", |_| {});
    h.reload(
        "[output]\nmode = \"1280x720@30000/1001\"\nformat = \"i420\"\n",
        0,
    )
    .unwrap();
    assert_eq!(h.snap().output_mode, "I420 1280x720@30000/1001");
    assert_eq!(h.world().writer_starts, 2);
}

#[test]
fn a_loopback_change_is_deferred_like_an_output_mode_change() {
    let mut h = H::new("", |_| {});
    h.consumers(1, 0);
    h.reload("[loopback]\nlabel = \"Other\"\n", 100).unwrap();
    assert_eq!(h.world().writer_starts, 1, "a consumer holds the device");
    h.world().sink = Ok(PathBuf::from("/dev/video12"));
    h.consumers(0, 200);
    assert_eq!(h.world().writer_starts, 2);
    assert_eq!(
        h.world().writer.as_ref().unwrap().0,
        PathBuf::from("/dev/video12")
    );
}

#[test]
fn a_missing_loopback_is_an_error_retried_every_two_seconds() {
    let mut h = H::new("", |w| {
        w.sink = Err("no v4l2loopback device labelled \"Omarchy Broadcast\"".into())
    });
    assert_eq!(h.state(), State::Error);
    assert!(h.snap().error.contains("Omarchy Broadcast"));
    assert_eq!(h.ctrl.next_wakeup(h.at(0)), WRITER_RETRY);
    h.world().sink = Ok(PathBuf::from("/dev/video11"));
    h.tick(1999);
    assert_eq!(h.state(), State::Error);
    assert_eq!(h.world().writer_starts, 0);
    h.tick(2000);
    assert_eq!(h.state(), State::Idle);
    assert_eq!(
        h.snap().error,
        "",
        "the writer's error is cleared once it runs"
    );
    assert_eq!(h.world().writer_starts, 1);
    // Disabled while the loopback is missing: off, and still retried.
    let mut h = H::new("", |w| w.sink = Err("held in YUY2".into()));
    h.call(|r| Command::SetEnabled(false, r), 0).unwrap();
    assert_eq!(h.state(), State::Off);
    h.world().sink = Ok(PathBuf::from("/dev/video11"));
    h.tick(2000);
    assert_eq!(h.world().writer_starts, 1);
}

#[test]
fn a_writer_failure_stops_the_session_and_is_retried() {
    let mut h = H::new("", |_| {});
    h.activate(0);
    h.world().writer_failure = Some("loopback writer failed: device gone".into());
    h.tick(1000);
    assert_eq!(h.state(), State::Error);
    assert_eq!(h.snap().consumers, 0);
    assert!(h.world().session.is_none() && h.world().writer.is_none());
    h.tick(2999);
    assert_eq!(h.world().writer_starts, 1);
    h.tick(3000);
    assert_eq!(h.state(), State::Idle);
    assert_eq!(h.world().writer_starts, 2);
}

#[test]
fn stats_flow_about_once_a_second_only_while_active() {
    let mut h = H::new("", |_| {});
    for ms in (0..3000).step_by(100) {
        h.tick(ms);
    }
    assert!(h.drain().1.is_empty(), "no stats while idle");
    h.activate(3000);
    h.drain();
    let taken = h.world().stats_taken;
    assert_eq!(taken, 1, "the window is reset at the first frame");
    for ms in (3300..=6300).step_by(50) {
        h.tick(ms);
    }
    let (_, stats) = h.drain();
    assert_eq!(stats.len(), 3, "{stats:?}");
    assert!(stats.iter().all(|s| s.fps == 30.0 && s.gpu_percent == -1.0));
    // Idle again: none.
    h.consumers(0, 6300);
    h.tick(11_300);
    assert_eq!(h.state(), State::Idle);
    h.drain();
    for ms in (11_300..15_000).step_by(100) {
        h.tick(ms);
    }
    assert!(h.drain().1.is_empty());
    assert!(h.ctrl.next_wakeup(h.at(15_000)) >= Duration::from_secs(30));
}

#[test]
fn every_transition_of_the_lifecycle_is_announced_in_order() {
    let mut h = H::new("", |w| w.frames = false);
    h.consumers(1, 0);
    h.tick(300);
    h.world().frames = true;
    h.tick(350);
    h.consumers(0, 1000);
    h.tick(6000);
    h.call(|r| Command::SetEnabled(false, r), 7000).unwrap();
    h.call(|r| Command::SetEnabled(true, r), 8000).unwrap();
    h.world().cameras.clear();
    h.rescan(9000);
    h.consumers(1, 9000);
    h.tick(9300);
    h.consumers(0, 9500);
    let (changes, _) = h.drain();
    let seen: Vec<String> = changes
        .iter()
        .map(|c| match c {
            Change::State(s) => s.to_string(),
            Change::Consumers(n) => format!("consumers={n}"),
            Change::Enabled(b) => format!("enabled={b}"),
            Change::Cameras(c) => format!("cameras={}", c.len()),
            Change::ActiveCamera(a) => format!("active={a}"),
            Change::Error(e) => format!("error={e}"),
            Change::OutputMode(m) => format!("mode={m}"),
            Change::Effect(e) => format!("effect={e}"),
            Change::BlurStrength(v) => format!("strength={v}"),
            Change::BackgroundImage(p) => format!("image={p}"),
            Change::LoopbackConsumers(n) => format!("loopback={n}"),
            Change::PipeWireConsumers(n) => format!("pipewire={n}"),
            Change::PipeWireOutput(s) => format!("pipewire_output={s}"),
        })
        .collect();
    assert_eq!(
        seen,
        [
            "consumers=1",
            "loopback=1",
            "starting",
            "active",
            "consumers=0",
            "loopback=0",
            "idle",
            "enabled=false",
            "off",
            "enabled=true",
            "idle",
            "cameras=0",
            "active=",
            "consumers=1",
            "loopback=1",
            "error=no camera is available",
            "error",
            "consumers=0",
            "loopback=0",
            "idle",
        ]
    );
}

#[test]
fn diff_lists_state_last() {
    let a = preliminary_snapshot(&Paths {
        config: "/nonexistent/c.toml".into(),
        state: "/nonexistent/s.toml".into(),
    });
    let mut b = a.clone();
    b.state = State::Active;
    b.consumers = 2;
    b.active_camera = "x".into();
    b.loopback_consumers = 1;
    b.pipewire_consumers = 1;
    b.pipewire_output = "streaming".into();
    assert_eq!(
        diff(&a, &b),
        [
            Change::ActiveCamera("x".into()),
            Change::Consumers(2),
            Change::LoopbackConsumers(1),
            Change::PipeWireConsumers(1),
            Change::PipeWireOutput("streaming".into()),
            Change::State(State::Active)
        ]
    );
    assert_eq!(
        [
            Change::LoopbackConsumers(0).name(),
            Change::PipeWireConsumers(0).name(),
            Change::PipeWireOutput(String::new()).name()
        ],
        ["LoopbackConsumers", "PipeWireConsumers", "PipeWireOutput"]
    );
    assert!(diff(&a, &a).is_empty());
}

/// Drives the controller the way [`run`] does, with a fake clock that
/// jumps to each wakeup: the number of ticks over `span` from `from_ms`
/// (capped at 10 000, which a spinning loop reaches at once).
fn ticks_over(h: &mut H, from_ms: u64, span: Duration) -> u32 {
    let mut now = h.at(from_ms);
    let end = now + span;
    let mut n = 0;
    while now < end && n < 10_000 {
        now += h.ctrl.next_wakeup(now);
        h.ctrl.tick(now);
        n += 1;
    }
    n
}

#[test]
fn off_with_a_consumer_does_not_spin() {
    let mut h = H::new("", |_| {});
    h.call(|r| Command::SetEnabled(false, r), 0).unwrap();
    h.consumers(1, 10);
    h.tick(400);
    assert_eq!(h.state(), State::Off);
    let w = h.ctrl.next_wakeup(h.at(1000));
    assert!(w >= Duration::from_secs(1), "off + consumer: {w:?}");
    let n = ticks_over(&mut h, 1000, Duration::from_secs(10));
    assert!(n <= 2, "{n} ticks in 10 s while off with a consumer");
    assert_eq!(h.state(), State::Off);
    assert_eq!(h.world().session_starts, 0);
}

#[test]
fn error_with_a_consumer_does_not_spin() {
    // No camera.
    let mut h = H::new("", |w| w.cameras.clear());
    h.consumers(1, 10);
    h.tick(400);
    assert_eq!(h.state(), State::Error);
    let w = h.ctrl.next_wakeup(h.at(1000));
    assert!(w >= Duration::from_secs(1), "error + consumer: {w:?}");
    let n = ticks_over(&mut h, 1000, Duration::from_secs(10));
    assert!(n <= 2, "{n} ticks in 10 s in error with a consumer");
    // A failed start.
    let mut h = H::new("", |w| w.fail_start = Some("CUDA failed".into()));
    h.activate_expecting_error();
    let n = ticks_over(&mut h, 1000, Duration::from_secs(10));
    assert!(n <= 2, "{n} ticks in 10 s after a failed start");
    assert_eq!(h.world().session_starts, 0);
    // A missing loopback is still retried every 2 s, no faster.
    let mut h = H::new("", |w| w.sink = Err("no loopback".into()));
    h.consumers(1, 10);
    let n = ticks_over(&mut h, 400, Duration::from_secs(10));
    assert!((4..=6).contains(&n), "{n} ticks in 10 s without a loopback");
}

#[test]
fn cameras_without_frames_end_in_error_naming_them_and_recover() {
    let mut h = H::new("", |w| w.frames = false);
    h.consumers(1, 0);
    h.tick(300);
    let mut t = 300;
    for _ in 0..6 {
        t += 10_000;
        h.tick(t);
    }
    let ingests = h.world().ingests.clone();
    assert_eq!(h.state(), State::Error, "after 60 s, ingests {ingests:?}");
    assert_eq!(ingests, ["usb-a", "usb-b"], "each camera tried once");
    let e = h.snap().error;
    assert!(
        e.contains("Alpha Cam (usb-a)") && e.contains("Beta Cam (usb-b)"),
        "{e}"
    );
    assert!(h.world().session.is_none(), "camera and CUDA released");
    assert!(ticks_over(&mut h, t, Duration::from_secs(10)) <= 2);
    // The same cameras again: no change, still error.
    h.rescan(t + 100);
    assert_eq!(h.state(), State::Error);
    assert_eq!(h.world().ingests.len(), 2);
    // A control call retries, from the first camera.
    h.world().frames = true;
    h.call(|r| Command::SetEnabled(true, r), t + 200).unwrap();
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.snap().error, "");
    assert_eq!(h.world().ingests, ["usb-a", "usb-b", "usb-a"]);

    // A camera set change retries too, and so does a reload.
    let mut h = H::new("", |w| w.frames = false);
    h.consumers(1, 0);
    h.tick(300);
    h.tick(10_300);
    h.tick(20_300);
    assert_eq!(h.state(), State::Error);
    h.fake.world().cameras = vec![beta()];
    h.rescan(21_000);
    assert_eq!(h.state(), State::Starting);
    assert_eq!(h.world().ingests, ["usb-a", "usb-b", "usb-b"]);
    h.tick(31_000);
    assert_eq!(h.state(), State::Error);
    h.reload("version = 1\n", 32_000).unwrap();
    assert_eq!(h.state(), State::Starting);
}

#[test]
fn a_camera_that_delivered_may_be_tried_again_after_another_fails() {
    let mut h = H::new("", |_| {});
    h.activate(0);
    // Alpha fails at runtime: Beta takes over and delivers.
    h.world().failure = Some(Failure::Camera("Alpha: streaming error".into()));
    h.tick(1000);
    assert_eq!(h.snap().active_camera, "usb-b");
    h.tick(1050);
    assert_eq!(h.state(), State::Active);
    // Beta fails later: Alpha (which had failed before Beta delivered) is
    // a candidate again.
    h.world().failure = Some(Failure::Camera("Beta: streaming error".into()));
    h.tick(2000);
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.world().ingests, ["usb-a", "usb-b", "usb-a"]);
}

#[test]
fn a_damaged_state_file_fails_closed_until_set_enabled() {
    let mut h = H::new("", |_| {});
    let damaged = "enabled = \"false\"\n";
    std::fs::create_dir_all(h.paths.state.parent().unwrap()).unwrap();
    std::fs::write(&h.paths.state, damaged).unwrap();
    // Served before the controller runs, too.
    let pre = preliminary_snapshot(&h.paths);
    assert_eq!((pre.state, pre.enabled), (State::Off, false));
    assert!(pre.error.contains("state.toml"), "{}", pre.error);
    h.restart();
    assert_eq!(h.state(), State::Off, "damaged state file: {:?}", h.snap());
    assert!(!h.snap().enabled);
    let e = h.snap().error;
    assert!(
        e.contains(&h.paths.state.display().to_string()) && e.contains("SetEnabled"),
        "{e}"
    );
    // A consumer does not start the camera.
    h.consumers(1, 0);
    h.tick(1000);
    assert_eq!(h.state(), State::Off);
    assert_eq!(h.world().session_starts, 0);
    // A selection applies but does not rewrite the file.
    match h.call(|r| Command::SelectCamera("usb-b".into(), r), 1100) {
        Err(IpcError::Failed(m)) => assert!(m.contains("SetEnabled"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(h.snap().active_camera, "usb-b");
    // A reload keeps the error: its cause is still there.
    h.reload("version = 1\n", 1200).unwrap();
    assert_eq!(h.snap().error, e);
    assert_eq!(std::fs::read_to_string(&h.paths.state).unwrap(), damaged);
    // Still damaged after another restart.
    h.restart();
    assert_eq!(h.state(), State::Off);
    // SetEnabled rewrites the file and clears the error.
    h.consumers(1, 0);
    h.call(|r| Command::SetEnabled(true, r), 2000).unwrap();
    assert_eq!(h.snap().error, "");
    assert!(h.snap().enabled);
    h.tick(2300);
    assert_eq!(h.state(), State::Active);
    assert!(h.persisted().enabled);
    // An unreadable state file (a directory) fails closed as well.
    let mut h = H::new("", |_| {});
    let _ = std::fs::remove_file(&h.paths.state);
    std::fs::create_dir_all(&h.paths.state).unwrap();
    h.restart();
    assert_eq!(h.state(), State::Off);
    assert!(!h.snap().error.is_empty());
}

#[test]
fn a_valid_reload_keeps_the_error_of_a_missing_writer() {
    let mut h = H::new("", |w| w.sink = Err("no loopback labelled X".into()));
    assert_eq!(h.state(), State::Error);
    h.reload("version = 1\n", 100).unwrap();
    assert_eq!(h.state(), State::Error);
    assert_eq!(h.snap().error, "no loopback labelled X");
    // An invalid config error is cleared by the next valid reload, the
    // writer's stays.
    assert!(h.reload("bogus = 1\n", 200).is_err());
    assert_ne!(h.snap().error, "no loopback labelled X");
    h.reload("version = 1\n", 300).unwrap();
    assert_eq!(h.snap().error, "no loopback labelled X");
    // With a fault only, a reload clears it.
    let mut h = H::new("", |w| w.cameras.clear());
    h.activate_expecting_error();
    h.reload("version = 1\n", 400).unwrap();
    assert_eq!(h.state(), State::Error, "still no camera: a new fault");
    h.consumers(0, 500);
    h.reload("version = 1\n", 600).unwrap();
    assert_eq!((h.state(), h.snap().error.as_str()), (State::Idle, ""));
}

#[test]
fn a_camera_whose_id_changes_between_scans_is_renamed_not_lost() {
    // The first scan ran before udev added the links: the id is the node.
    let early = cam("/dev/video2", "Beta Cam", Some("2222:0002"), "/dev/video2");
    let mut h = H::new("", |w| w.cameras = vec![early.clone()]);
    h.call(|r| Command::SelectCamera("/dev/video2".into(), r), 0)
        .unwrap();
    assert_eq!(h.persisted().camera.as_deref(), Some("/dev/video2"));
    h.activate(0);
    assert_eq!(h.snap().active_camera, "/dev/video2");
    h.drain();
    // The second scan sees the by-path/by-id name.
    h.fake.world().cameras = vec![beta()];
    h.rescan(3000);
    assert_eq!(h.state(), State::Active, "{:?}", h.snap());
    assert_eq!(h.snap().active_camera, "usb-b");
    assert_eq!(ids(&h.snap()), ["usb-b"]);
    {
        let w = h.world();
        assert_eq!(w.ingests, ["/dev/video2"], "no camera switch");
        assert_eq!((w.session_starts, w.session_stops), (1, 0));
    }
    assert_eq!(h.snap().error, "");
    assert_eq!(
        h.persisted().camera.as_deref(),
        Some("usb-b"),
        "the selection follows the camera"
    );
    let changes = h.drain().0;
    assert!(
        !changes.iter().any(|c| matches!(c, Change::State(_))),
        "{changes:?}"
    );
    // The same while idle: the selection follows.
    let mut h = H::new("", |w| w.cameras = vec![alpha(), early.clone()]);
    h.call(|r| Command::SelectCamera("/dev/video2".into(), r), 0)
        .unwrap();
    h.fake.world().cameras = vec![alpha(), beta()];
    h.rescan(1000);
    assert_eq!(h.snap().active_camera, "usb-b");
    h.activate(2000);
    assert_eq!(h.world().ingests, ["usb-b"]);
}

/// A USB camera at `node` with a serial number and a port.
fn usb_cam(id: &str, usb: &str, serial: &str, port: &str, node: &str) -> Camera {
    let mut c = cam(id, "Cam", Some(usb), node);
    c.serial = Some(serial.into());
    c.usb_path = Some(port.into());
    c
}

#[test]
fn a_node_reused_by_another_camera_is_no_rename() {
    let first = usb_cam("usb-Cam_S1", "1111:0001", "S1", "3-4", "/dev/video2");
    // Unplugged, and another camera plugged in, between two scans: the
    // newcomer gets the same node.
    let other = usb_cam("usb-Other_S9", "3333:0003", "S9", "3-4", "/dev/video2");
    let mut h = H::new("", |w| w.cameras = vec![alpha(), first.clone()]);
    h.call(|r| Command::SelectCamera("usb-Cam_S1".into(), r), 0)
        .unwrap();
    h.fake.world().cameras = vec![alpha(), other.clone()];
    h.rescan(1000);
    assert_eq!(ids(&h.snap()), ["usb-a", "usb-Other_S9"]);
    assert_eq!(
        h.persisted().camera.as_deref(),
        Some("usb-Cam_S1"),
        "the selection stays on the camera that left"
    );
    assert_eq!(h.snap().active_camera, "usb-a", "first in order meanwhile");
    // It comes back (at another node): it is the choice again.
    let mut back = first.clone();
    back.path = "/dev/video4".into();
    h.fake.world().cameras = vec![alpha(), other.clone(), back];
    h.rescan(2000);
    assert_eq!(h.snap().active_camera, "usb-Cam_S1");

    // The same while streaming from it: its loss is a loss, and the
    // newcomer at its node may take over.
    let mut h = H::new("", |w| w.cameras = vec![first.clone()]);
    h.call(|r| Command::SelectCamera("usb-Cam_S1".into(), r), 0)
        .unwrap();
    h.activate(0);
    h.fake.world().cameras = vec![other.clone()];
    h.rescan(3000);
    h.tick(3300);
    assert_eq!(h.state(), State::Active, "{:?}", h.snap());
    assert_eq!(h.world().ingests, ["usb-Cam_S1", "usb-Other_S9"]);
    assert_eq!(h.snap().active_camera, "usb-Other_S9");
    assert_eq!(h.persisted().camera.as_deref(), Some("usb-Cam_S1"));

    // A camera known by its node only that a different model replaced: no
    // rename either.
    let early = cam("/dev/video2", "Cam", Some("1111:0001"), "/dev/video2");
    let mut h = H::new("", |w| w.cameras = vec![alpha(), early]);
    h.call(|r| Command::SelectCamera("/dev/video2".into(), r), 0)
        .unwrap();
    h.fake.world().cameras = vec![alpha(), other];
    h.rescan(1000);
    assert_eq!(h.persisted().camera.as_deref(), Some("/dev/video2"));
    assert_eq!(h.snap().active_camera, "usb-a");
}

#[test]
fn the_same_device_under_a_new_id_is_renamed() {
    // Its by-path name first, its by-id name once udev added that link:
    // the same serial at the same port.
    let by_path = usb_cam(
        "pci-0:14.0-usb-0:4:1.0-video-index0",
        "1111:0001",
        "S1",
        "3-4",
        "/dev/video2",
    );
    let by_id = usb_cam(
        "usb-Cam_S1-video-index0",
        "1111:0001",
        "S1",
        "3-4",
        "/dev/video2",
    );
    let mut h = H::new("", |w| w.cameras = vec![alpha(), by_path.clone()]);
    h.call(|r| Command::SelectCamera(by_path.id.clone(), r), 0)
        .unwrap();
    h.activate(0);
    h.drain();
    h.fake.world().cameras = vec![alpha(), by_id.clone()];
    h.rescan(3000);
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.snap().active_camera, by_id.id);
    assert_eq!(h.persisted().camera.as_deref(), Some(by_id.id.as_str()));
    {
        let w = h.world();
        assert_eq!(w.ingests, [by_path.id.as_str()], "no camera switch");
        assert_eq!((w.session_starts, w.session_stops), (1, 0));
    }
    let changes = h.drain().0;
    assert!(
        !changes.iter().any(|c| matches!(c, Change::State(_))),
        "{changes:?}"
    );
}

#[test]
fn cameras_are_listed_under_clean_unique_names() {
    let twin = |id: &str, node: &str| {
        let mut c = cam(id, "x", Some("1111:0001"), node);
        c.card = "Desk Cam: Desk Cam".into();
        c
    };
    let mut h = H::new("", |w| {
        w.cameras = vec![
            twin("usb-Desk_S2", "/dev/video0"),
            twin("usb-Desk_S1", "/dev/video2"),
        ]
    });
    let names: Vec<String> = h.snap().cameras.iter().map(|c| c.name.clone()).collect();
    assert_eq!(names, ["Desk Cam (2)", "Desk Cam"]);
    // Selecting by the numbered name picks that camera; the selection is
    // saved by id.
    h.call(|r| Command::SelectCamera("desk cam (2)".into(), r), 0)
        .unwrap();
    assert_eq!(h.persisted().camera.as_deref(), Some("usb-Desk_S2"));
}

#[test]
fn a_disable_requested_during_the_output_build_never_opens_the_camera() {
    let mut h = H::new("", |_| {});
    // SetEnabled(false) arrives (the D-Bus side counts it, then queues it)
    // while the output is being built.
    let published = h.ctrl.published.clone();
    h.world().during_start = Some(Hook(Box::new(move || published.request_disable())));
    h.consumers(1, 0);
    h.tick(300);
    {
        let w = h.world();
        assert_eq!(w.session_starts, 1, "the output was built");
        assert!(
            w.ingests.is_empty(),
            "no camera was opened: {:?}",
            w.ingests
        );
        assert!(w.session.is_none(), "the output is released");
    }
    assert_eq!(h.snap().error, "", "an abort is not a fault");
    // Not retried while the command is queued, and no spin meanwhile.
    assert!(ticks_over(&mut h, 300, Duration::from_secs(10)) <= 2);
    assert_eq!(h.world().session_starts, 1);
    // The queued command then completes normally.
    h.call(|r| Command::SetEnabled(false, r), 10_400).unwrap();
    assert_eq!(h.state(), State::Off);
    assert!(!h.persisted().enabled);
    assert!(h.world().ingests.is_empty());
    // Handled: enabling again starts the camera at once (the consumer is
    // still there).
    h.world().during_start = None;
    h.call(|r| Command::SetEnabled(true, r), 10_500).unwrap();
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.world().ingests, ["usb-a"]);
}

#[test]
fn a_writer_rebuild_recounts_the_holders_and_waits_for_them() {
    let mut h = H::new("", |_| {});
    // An application has just opened the loopback; the watcher has not
    // counted it yet.
    h.world().holders = 1;
    h.reload("[output]\nmode = \"1280x720@30\"\n", 0).unwrap();
    assert_eq!(h.world().writer_starts, 1, "deferred: the device is held");
    assert!(h.world().holder_counts >= 1, "counted before the rebuild");
    assert_eq!(h.snap().output_mode, "NV12 1920x1080@30");
    // Counted again every WRITER_RETRY until it is released.
    assert_eq!(h.ctrl.next_wakeup(h.at(0)), WRITER_RETRY);
    h.tick(2000);
    assert_eq!(h.world().writer_starts, 1);
    assert_eq!(h.ctrl.next_wakeup(h.at(2000)), WRITER_RETRY);
    // Released, with no count from the watcher in between: rebuilt at the
    // next recount.
    h.world().holders = 0;
    h.tick(4000);
    assert_eq!(h.world().writer_starts, 2);
    assert_eq!(h.snap().output_mode, "NV12 1280x720@30");
    assert!(h.ctrl.next_wakeup(h.at(4000)) >= Duration::from_secs(30));

    // A loopback change, held when the watcher's last consumer leaves.
    let mut h = H::new("", |_| {});
    h.consumers(1, 0);
    h.reload("[loopback]\nlabel = \"Other\"\n", 100).unwrap();
    h.world().holders = 1;
    h.consumers(0, 200);
    assert_eq!(h.world().writer_starts, 1, "another process still holds it");
    h.world().holders = 0;
    h.world().sink = Ok(PathBuf::from("/dev/video12"));
    h.tick(2200);
    assert_eq!(h.world().writer_starts, 2);
    assert_eq!(
        h.world().writer.as_ref().unwrap().0,
        PathBuf::from("/dev/video12")
    );
}

// ---- effects (docs/adr/0012-video-effects-and-maxine-backend.md) ----

impl H {
    fn set_effect(&mut self, name: &str, ms: u64) -> Result<(), IpcError> {
        let name = name.to_string();
        self.call(|r| Command::SetEffect(name, r), ms)
    }

    fn set_strength(&mut self, v: f64, ms: u64) -> Result<(), IpcError> {
        self.call(|r| Command::SetBlurStrength(v, r), ms)
    }

    fn set_image(&mut self, path: &str, ms: u64) -> Result<(), IpcError> {
        let path = path.to_string();
        self.call(|r| Command::SetBackgroundImage(path, r), ms)
    }

    /// A background image file in the test's directory.
    fn image(&self) -> String {
        let p = self._dir.path().join("office.png");
        fs::write(&p, b"an image").unwrap();
        p.display().to_string()
    }

    /// The effect the running session applies.
    fn applied(&self) -> Option<EffectMode> {
        self.world().effect.as_ref().map(|e| e.mode)
    }
}

fn effect_changes(changes: &[Change]) -> Vec<String> {
    changes
        .iter()
        .filter_map(|c| match c {
            Change::Effect(e) => Some(format!("effect={e}")),
            Change::BlurStrength(v) => Some(format!("strength={v}")),
            Change::BackgroundImage(p) => Some(format!("image={p}")),
            _ => None,
        })
        .collect()
}

#[test]
fn the_effect_methods_persist_publish_and_survive_a_restart() {
    let mut h = H::new("", |_| {});
    let s = h.snap();
    assert_eq!(
        (
            s.effect.as_str(),
            s.blur_strength,
            s.background_image.as_str()
        ),
        ("none", 0.5, "")
    );
    let image = h.image();
    h.set_strength(0.2, 0).unwrap();
    h.set_image(&image, 0).unwrap();
    h.set_effect("replace", 0).unwrap();
    let s = h.snap();
    assert_eq!(
        (
            s.effect.as_str(),
            s.blur_strength,
            s.background_image.as_str()
        ),
        ("replace", 0.2, image.as_str())
    );
    assert_eq!(
        effect_changes(&h.drain().0),
        [
            "strength=0.2".to_string(),
            format!("image={image}"),
            "effect=replace".into()
        ]
    );
    let st = h.persisted();
    assert_eq!(st.effect, Some(EffectMode::Replace));
    assert_eq!(st.blur_strength, Some(0.2));
    assert_eq!(st.background_image, Some(PathBuf::from(&image)));
    assert!(st.enabled, "the other keys are kept");
    // Setting the same value again announces nothing.
    h.set_effect("replace", 0).unwrap();
    assert!(effect_changes(&h.drain().0).is_empty());
    // A restart serves the saved effect, before and after the controller
    // runs.
    let pre = preliminary_snapshot(&h.paths);
    assert_eq!((pre.effect.as_str(), pre.blur_strength), ("replace", 0.2));
    h.restart();
    assert_eq!(h.snap().effect, "replace");
    assert_eq!(h.snap().background_image, image);
    // An empty image removes the run-time image (once another effect is
    // selected; the config has none to fall back to), and the state file no
    // longer holds one.
    h.set_effect("blur", 0).unwrap();
    h.set_image("", 0).unwrap();
    assert_eq!(h.snap().background_image, "");
    assert_eq!(h.persisted().background_image, None);
}

#[test]
fn clearing_the_image_brings_back_the_configs_image() {
    let cfg_image = "/cfg/office.png";
    let config = format!("[effect]\nmode = \"replace\"\nbackground_image = \"{cfg_image}\"\n");
    let mut h = H::new(&config, |_| {});
    let image = h.image();
    h.set_image(&image, 0).unwrap();
    assert_eq!(h.snap().background_image, image);
    // Clearing is allowed under replace: the config's image takes over.
    h.set_image("", 0).unwrap();
    assert_eq!(h.snap().background_image, cfg_image);
    assert_eq!(h.snap().effect, "replace");
    assert_eq!(h.persisted().background_image, None);
    // An empty image in the state file (as an earlier daemon wrote for a
    // cleared one) is no override: the config's image applies, no error.
    fs::write(
        &h.paths.state,
        "enabled = true\neffect = \"replace\"\nbackground_image = \"\"\n",
    )
    .unwrap();
    h.restart();
    assert_eq!(h.snap().background_image, cfg_image);
    assert_eq!(h.snap().error, "");
    h.activate(1000);
    assert_eq!(h.applied(), Some(EffectMode::Replace));
    assert_eq!(h.snap().error, "");
    // A reload that adds a config image under a state that never set one
    // uses it.
    let mut h = H::new("", |_| {});
    h.set_effect("blur", 0).unwrap();
    h.set_image("", 0).unwrap();
    h.reload(&config, 1000).unwrap();
    assert_eq!(h.snap().background_image, cfg_image);
}

#[test]
fn a_repeated_setting_neither_rewrites_the_state_nor_reloads_the_effect() {
    let mut h = H::new("[effect]\nmode = \"blur\"\n", |_| {});
    h.activate(0);
    h.tick(500);
    assert_eq!(h.ctrl.effect_seen, Some(EffectStatus::Ready));
    // A strength change reaches the session; the effect stays ready (its
    // load is not reported again).
    h.set_effect("blur", 900).unwrap();
    assert_eq!(h.ctrl.effect_seen, Some(EffectStatus::Ready));
    h.set_strength(0.4, 1000).unwrap();
    assert_eq!(h.persisted().blur_strength, Some(0.4));
    assert_eq!(h.ctrl.effect_seen, Some(EffectStatus::Ready));
    let sets = h.world().effect_sets.len();
    assert_eq!(h.world().effect.as_ref().unwrap().blur_strength, 0.4);
    // The same strength again: no write, no call to the session.
    fs::remove_file(&h.paths.state).unwrap();
    h.set_strength(0.4, 1100).unwrap();
    h.set_effect("blur", 1200).unwrap();
    h.tick(1500);
    assert!(!h.paths.state.exists(), "the state file was rewritten");
    assert_eq!(h.world().effect_sets.len(), sets);
    assert_eq!(h.ctrl.effect_seen, Some(EffectStatus::Ready));
    // A reload of the same config does not reload the effect either.
    h.reload("[effect]\nmode = \"blur\"\n", 2000).unwrap();
    assert_eq!(h.world().effect_sets.len(), sets);
    assert_eq!(h.ctrl.effect_seen, Some(EffectStatus::Ready));
    // Another effect is saved, given to the session and followed.
    h.set_effect("none", 3000).unwrap();
    assert!(h.paths.state.exists());
    assert_eq!(h.world().effect_sets.len(), sets + 1);
    assert_eq!(h.ctrl.effect_seen, Some(EffectStatus::Off));
}

#[test]
fn an_effect_error_with_none_ends_with_the_session() {
    let config = "[effect]\nmode = \"blur\"\non_failure = \"none\"\n\
                  [lifecycle]\nidle_grace_s = 0.0\nstart_debounce_ms = 0\n";
    let mut h = H::new(config, |_| {});
    h.activate(0);
    h.world().effect_status = EffectStatus::Failed("processing a frame failed".into());
    h.tick(1000);
    assert_eq!(h.state(), State::Active);
    assert!(h.snap().error.contains("without the effect"));
    // The consumer leaves: idle, and the effect's error is gone with it.
    h.consumers(0, 2000);
    h.tick(3000);
    assert_eq!(h.state(), State::Idle);
    assert_eq!(h.snap().error, "");
    // Off ends it too.
    h.activate(4000);
    h.world().effect_status = EffectStatus::Failed("processing a frame failed".into());
    h.tick(5000);
    assert!(h.snap().error.contains("without the effect"));
    h.call(|r| Command::SetEnabled(false, r), 6000).unwrap();
    assert_eq!(h.state(), State::Off);
    assert_eq!(h.snap().error, "");
}

#[test]
fn a_fifo_in_place_of_the_image_is_refused_without_blocking() {
    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("bg.png");
    let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: mkfifo(3) on a NUL-terminated path we own.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let (tx, rx) = std::sync::mpsc::channel();
    let path = fifo.clone();
    std::thread::spawn(move || {
        let _ = tx.send(check_background_file(&path));
    });
    // A blocking open would wait for a writer forever.
    let got = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("checking a FIFO blocked");
    let e = got.unwrap_err();
    assert!(e.contains("not a regular file"), "{e}");
    // A regular file is still accepted, an absent one is unreadable.
    let file = dir.path().join("ok.png");
    fs::write(&file, b"x").unwrap();
    check_background_file(&file).unwrap();
    let e = check_background_file(&dir.path().join("none.png")).unwrap_err();
    assert!(e.contains("unreadable"), "{e}");
}

#[test]
fn the_state_file_takes_precedence_over_the_config_across_reloads() {
    let image = "/cfg/office.png";
    let config = format!(
        "[effect]\nmode = \"replace\"\nblur_strength = 0.9\nbackground_image = \"{image}\"\n"
    );
    let mut h = H::new(&config, |_| {});
    let s = h.snap();
    assert_eq!(
        (
            s.effect.as_str(),
            s.blur_strength,
            s.background_image.as_str()
        ),
        ("replace", 0.9, image)
    );
    h.set_effect("blur", 0).unwrap();
    assert_eq!(h.snap().effect, "blur");
    // A reload keeps the state's effect, and takes the config's keys the
    // state does not set.
    h.reload(
        &format!(
            "[effect]\nmode = \"none\"\nblur_strength = 0.3\nbackground_image = \"{image}\"\n"
        ),
        1000,
    )
    .unwrap();
    assert_eq!(h.snap().effect, "blur");
    assert_eq!(h.snap().blur_strength, 0.3);
    // While active, a reload that changes the effect in force applies it
    // to the session without restarting it.
    h.activate(2000);
    assert_eq!(h.applied(), Some(EffectMode::Blur));
    assert_eq!(
        h.world().session.as_ref().unwrap().0.effect.blur_strength,
        0.3
    );
    h.reload("[effect]\nblur_strength = 0.6\n", 3000).unwrap();
    assert_eq!(h.snap().blur_strength, 0.6);
    let w = h.world();
    assert_eq!(w.session_starts, 1);
    assert_eq!(w.effect.as_ref().unwrap().blur_strength, 0.6);
}

#[test]
fn invalid_effect_arguments_are_refused_and_change_nothing() {
    let mut h = H::new("", |_| {});
    let image = h.image();
    let invalid = |r: Result<(), IpcError>| match r {
        Err(IpcError::InvalidArgument(m)) => m,
        other => panic!("expected InvalidArgument, got {other:?}"),
    };
    assert!(invalid(h.set_effect("sparkle", 0)).contains("unknown effect"));
    assert!(invalid(h.set_effect("replace", 0)).contains("needs a background image"));
    for v in [-0.01, 1.01, f64::NAN, f64::NEG_INFINITY] {
        assert!(
            invalid(h.set_strength(v, 0)).contains("outside 0 to 1"),
            "{v}"
        );
    }
    assert!(invalid(h.set_image("office.png", 0)).contains("not absolute"));
    let dir = h._dir.path().display().to_string();
    assert!(invalid(h.set_image(&dir, 0)).contains("not a regular file"));
    assert!(invalid(h.set_image("/no/such/file.png", 0)).contains("unreadable"));
    // Larger than 32 MiB (a sparse file).
    let big = h._dir.path().join("big.png");
    fs::File::create(&big)
        .unwrap()
        .set_len(MAX_BACKGROUND_BYTES + 1)
        .unwrap();
    assert!(invalid(h.set_image(&big.display().to_string(), 0)).contains("32 MiB"));
    // Exactly the limit is accepted.
    let edge = h._dir.path().join("edge.png");
    fs::File::create(&edge)
        .unwrap()
        .set_len(MAX_BACKGROUND_BYTES)
        .unwrap();
    h.set_image(&edge.display().to_string(), 0).unwrap();
    h.set_image(&image, 0).unwrap();
    h.set_effect("replace", 0).unwrap();
    assert!(invalid(h.set_image("", 0)).contains("select another effect"));
    // Nothing of the refused calls reached the state file.
    let st = h.persisted();
    assert_eq!(st.effect, Some(EffectMode::Replace));
    assert_eq!(st.blur_strength, None);
    assert_eq!(st.background_image, Some(PathBuf::from(&image)));
}

#[test]
fn switching_effects_while_active_never_restarts_the_session() {
    let mut h = H::new("", |_| {});
    let image = h.image();
    h.activate(0);
    assert_eq!(h.applied(), Some(EffectMode::None));
    h.drain();
    h.set_effect("blur", 1000).unwrap();
    h.set_strength(0.8, 1100).unwrap();
    h.set_image(&image, 1200).unwrap();
    h.set_effect("replace", 1300).unwrap();
    h.set_effect("none", 1400).unwrap();
    h.set_effect("blur", 1500).unwrap();
    h.tick(2000);
    assert_eq!(h.state(), State::Active);
    assert!(h.states().is_empty(), "the state never left active");
    let w = h.world();
    assert_eq!((w.session_starts, w.session_stops), (1, 0));
    assert_eq!(w.ingests, ["usb-a"], "the camera was not reopened");
    let modes: Vec<EffectMode> = w.effect_sets.iter().map(|e| e.mode).collect();
    use EffectMode::{Blur, None as Off, Replace};
    assert_eq!(modes, [Blur, Blur, Blur, Replace, Off, Blur]);
    assert_eq!(w.effect_sets[1].blur_strength, 0.8);
    assert_eq!(
        w.effect_sets[3].background_image,
        Some(PathBuf::from(&image))
    );
}

#[test]
fn off_and_idle_stop_the_session_whatever_the_effect() {
    let mut h = H::new("[effect]\nmode = \"blur\"\n", |_| {});
    h.activate(0);
    assert_eq!(h.applied(), Some(EffectMode::Blur));
    h.call(|r| Command::SetEnabled(false, r), 1000).unwrap();
    assert_eq!(h.state(), State::Off);
    assert!(h.world().session.is_none());
    assert_eq!(h.snap().effect, "blur", "the selection stays");
    h.call(|r| Command::SetEnabled(true, r), 2000).unwrap();
    h.tick(2300);
    assert_eq!(h.state(), State::Active);
    h.consumers(0, 3000);
    h.tick(3000 + 5000);
    assert_eq!(h.state(), State::Idle);
    assert!(h.world().session.is_none());
    assert_eq!(h.world().effect, None, "the models went with the session");
    // An effect selected while idle starts nothing.
    h.set_effect("none", 9000).unwrap();
    assert_eq!(h.world().session_starts, 2);
}

#[test]
fn an_unavailable_effect_with_black_is_an_error_before_the_camera_opens() {
    let mut h = H::new("[effect]\nmode = \"blur\"\n", |w| {
        w.effect_unavailable = Some("libVideoFX.so not found".into())
    });
    h.activate_expecting_error();
    assert_eq!(
        h.snap().error,
        "effect blur unavailable: libVideoFX.so not found"
    );
    {
        let w = h.world();
        assert_eq!(w.session_starts, 0, "no output was built");
        assert!(w.ingests.is_empty(), "no camera was opened");
        assert_eq!(w.effect_probes, 1);
    }
    // No retry by itself, and no spinning.
    h.tick(10_000);
    assert_eq!(h.world().effect_probes, 1);
    // A control call retries: still unavailable, still error.
    h.set_effect("blur", 11_000).unwrap();
    assert_eq!(h.state(), State::Error);
    assert_eq!(h.world().effect_probes, 2);
    // Selecting no effect: the camera starts.
    h.set_effect("none", 12_000).unwrap();
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.snap().error, "");
    assert_eq!(h.applied(), Some(EffectMode::None));
}

#[test]
fn an_unavailable_effect_with_none_streams_without_it_and_says_why() {
    let mut h = H::new("[effect]\nmode = \"blur\"\non_failure = \"none\"\n", |w| {
        w.effect_unavailable = Some("libVideoFX.so not found".into())
    });
    h.activate(0);
    assert_eq!(h.applied(), Some(EffectMode::None), "started without it");
    let e = h.snap().error;
    assert!(
        e.starts_with("effect blur unavailable: libVideoFX.so not found;")
            && e.contains("without the effect"),
        "{e}"
    );
    assert_eq!(h.snap().effect, "blur", "the selection is still blur");
    h.tick(2000);
    assert_eq!(h.snap().error, e, "the error stays while it streams");
    // A replace with no image cannot run either: the same rule.
    let mut h = H::new("[effect]\non_failure = \"none\"\n", |_| {});
    h.runtime_effect(EffectMode::Replace);
    h.activate(0);
    assert!(
        h.snap().error.contains("needs a background image"),
        "{}",
        h.snap().error
    );
}

impl H {
    /// Puts `mode` in the state file, as an earlier run would have, and
    /// restarts on it.
    fn runtime_effect(&mut self, mode: EffectMode) {
        RuntimeState {
            effect: Some(mode),
            ..RuntimeState::default()
        }
        .save(&self.paths.state)
        .unwrap();
        self.restart();
    }
}

#[test]
fn an_effect_failure_with_black_stops_the_session_in_error_and_a_call_retries() {
    let mut h = H::new("[effect]\nmode = \"blur\"\n", |w| w.effect_slow = true);
    h.activate(0);
    assert_eq!(h.world().effect_status, EffectStatus::Loading);
    h.world().effect_status = EffectStatus::Ready;
    h.tick(500);
    h.drain();
    // The effect fails while streaming (a frame, the GPU).
    h.world().effect_status = EffectStatus::Failed("processing a frame failed".into());
    h.tick(1000);
    assert_eq!(h.state(), State::Error);
    assert_eq!(
        h.snap().error,
        "effect blur unavailable: processing a frame failed"
    );
    assert!(h.world().session.is_none(), "camera closed, GPU released");
    assert_eq!(h.world().writer_starts, 1, "the writer keeps the stream");
    assert_eq!(h.states(), [State::Error]);
    // Nothing restarts by itself while the consumer stays.
    h.tick(20_000);
    assert_eq!(h.world().session_starts, 1);
    // A control call leaves error: the session starts again.
    h.set_strength(0.4, 21_000).unwrap();
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.world().session_starts, 2);
    assert_eq!(h.snap().error, "");
    // A failure at load, at start, is an error in the same step.
    let mut h = H::new("[effect]\nmode = \"blur\"\n", |w| {
        w.effect_fail = Some("the model did not load".into())
    });
    h.activate_expecting_error();
    assert_eq!(
        h.snap().error,
        "effect blur unavailable: the model did not load"
    );
    assert!(h.world().session.is_none());
    // The consumer leaves: idle, and back: tried again.
    h.consumers(0, 1000);
    assert_eq!(h.state(), State::Idle);
    h.world().effect_fail = None;
    h.activate(2000);
    assert_eq!(h.snap().error, "");
}

#[test]
fn an_effect_failure_with_none_keeps_streaming_with_the_reason_in_error() {
    let mut h = H::new("[effect]\nmode = \"blur\"\non_failure = \"none\"\n", |_| {});
    h.activate(0);
    h.drain();
    h.world().effect_status = EffectStatus::Failed("processing a frame failed".into());
    h.tick(1000);
    assert_eq!(h.state(), State::Active);
    assert!(h.world().session.is_some());
    let e = h.snap().error;
    assert!(
        e.starts_with("effect blur unavailable: processing a frame failed;"),
        "{e}"
    );
    // Reported once, not on every poll.
    h.tick(1250);
    h.tick(1500);
    let errors = h
        .drain()
        .0
        .into_iter()
        .filter(|c| matches!(c, Change::Error(_)))
        .count();
    assert_eq!(errors, 1);
    // A retry that fails the same way is reported again.
    h.world().effect_fail = Some("processing a frame failed".into());
    h.set_effect("blur", 1800).unwrap();
    assert_eq!(h.snap().error, e, "the same failure, reported again");
    h.world().effect_fail = None;
    // A call retries the effect on the running session; ready clears it.
    h.set_effect("blur", 2000).unwrap();
    assert_eq!(h.snap().error, "");
    assert_eq!(h.world().session_starts, 1);
    assert_eq!(h.world().effect_status, EffectStatus::Ready);
    // A setting the session refuses (the image vanished): Failed, and the
    // stream goes on without the effect.
    let image = h.image();
    h.set_image(&image, 3000).unwrap();
    h.world().fail_set_effect = Some("the background image is unreadable".into());
    match h.set_effect("replace", 3100) {
        Err(IpcError::Failed(m)) => assert!(m.contains("unreadable"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.applied(), Some(EffectMode::None));
    assert!(h.snap().error.contains("unreadable"));
}

#[test]
fn a_setting_the_session_refuses_with_black_is_an_error() {
    let mut h = H::new("", |_| {});
    let image = h.image();
    h.set_image(&image, 0).unwrap();
    h.activate(0);
    h.world().fail_set_effect = Some("the background image is unreadable".into());
    match h.set_effect("replace", 1000) {
        Err(IpcError::Failed(m)) => assert_eq!(
            m,
            "effect replace unavailable: the background image is unreadable"
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(h.state(), State::Error);
    assert!(h.world().session.is_none());
    assert_eq!(h.persisted().effect, Some(EffectMode::Replace), "saved");
}

#[test]
fn an_effect_value_the_daemon_does_not_accept_fails_closed() {
    let mut h = H::new("", |_| {});
    std::fs::create_dir_all(h.paths.state.parent().unwrap()).unwrap();
    std::fs::write(&h.paths.state, "enabled = true\neffect = \"sparkle\"\n").unwrap();
    h.restart();
    assert_eq!(h.state(), State::Off);
    assert!(
        h.snap().error.contains("unknown effect"),
        "{}",
        h.snap().error
    );
    h.consumers(1, 0);
    h.tick(1000);
    assert_eq!(h.world().session_starts, 0, "no raw image instead");
}

// ---- the PipeWire output ----

const PIPEWIRE: &str = "[pipewire]\nenabled = true\n";
const PIPEWIRE_720P: &str = "[pipewire]\nenabled = true\n[output]\nmode = \"1280x720@30\"\n";

impl H {
    /// An event of the PipeWire output of `generation`.
    fn pipewire_gen(&mut self, generation: PipeWireGeneration, event: PipeWireEvent, ms: u64) {
        let now = self.at(ms);
        assert!(self.ctrl.handle(Command::PipeWire(generation, event), now));
    }

    /// An event of the PipeWire output started last (it may no longer
    /// run).
    fn pipewire(&mut self, event: PipeWireEvent, ms: u64) {
        let generation = self.world().pipewire_generation;
        self.pipewire_gen(generation, event, ms);
    }

    fn pw_consumers(&mut self, n: u32, ms: u64) {
        self.pipewire(PipeWireEvent::Consumers(n), ms);
    }

    fn pw_state(&mut self, state: PipeWireState, ms: u64) {
        self.pipewire(PipeWireEvent::State(state), ms);
    }

    /// (`Consumers`, `LoopbackConsumers`, `PipeWireConsumers`,
    /// `PipeWireOutput`).
    fn counts(&self) -> (u32, u32, u32, String) {
        let s = self.snap();
        (
            s.consumers,
            s.loopback_consumers,
            s.pipewire_consumers,
            s.pipewire_output,
        )
    }

    /// (writer starts, PipeWire output starts, the running PipeWire
    /// output's generation).
    fn outputs(&self) -> (u32, u32, Option<PipeWireGeneration>) {
        let w = self.world();
        (w.writer_starts, w.pipewire_starts, w.pipewire)
    }

    /// What a newly started PipeWire output reports first.
    fn pipewire_up(&mut self, ms: u64) {
        self.pw_state(PipeWireState::Connecting, ms);
        self.pw_consumers(0, ms);
        self.pw_state(PipeWireState::Ready, ms);
    }
}

fn counts(total: u32, loopback: u32, pipewire: u32, output: &str) -> (u32, u32, u32, String) {
    (total, loopback, pipewire, output.into())
}

#[test]
fn the_pipewire_output_starts_with_the_writer_when_configured() {
    let h = H::new("", |_| {});
    assert_eq!(h.outputs(), (1, 0, None));
    assert_eq!(h.counts(), counts(0, 0, 0, "off"));

    let mut h = H::new(PIPEWIRE, |_| {});
    assert_eq!(h.outputs(), (1, 1, Some(1)));
    // Until the output reports, it is connecting.
    assert_eq!(h.counts(), counts(0, 0, 0, "connecting"));
    h.pipewire_up(0);
    assert_eq!(h.counts(), counts(0, 0, 0, "ready"));
    let (changes, _) = h.drain();
    assert_eq!(changes, [Change::PipeWireOutput("ready".into())]);
}

#[test]
fn a_pipewire_consumer_starts_and_stops_the_camera_like_a_loopback_one() {
    let mut h = H::new(PIPEWIRE, |w| w.frames = false);
    h.pipewire_up(0);
    h.drain();
    h.pw_consumers(1, 1000);
    assert_eq!(h.counts(), counts(1, 0, 1, "ready"));
    h.tick(1299);
    assert_eq!(h.state(), State::Idle, "the debounce");
    h.tick(1300);
    assert_eq!(h.state(), State::Starting);
    assert_eq!(h.world().session_starts, 1);
    h.world().frames = true;
    h.pw_state(PipeWireState::Streaming, 1350);
    h.tick(1350);
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.counts(), counts(1, 0, 1, "streaming"));
    let (changes, _) = h.drain();
    assert_eq!(
        changes
            .iter()
            .filter(|c| !matches!(c, Change::ActiveCamera(_)))
            .cloned()
            .collect::<Vec<_>>(),
        [
            Change::Consumers(1),
            Change::PipeWireConsumers(1),
            Change::State(State::Starting),
            Change::PipeWireOutput("streaming".into()),
            Change::State(State::Active),
        ]
    );

    // It leaves: idle after the grace, the camera and CUDA stopped.
    h.pw_consumers(0, 2000);
    h.pw_state(PipeWireState::Ready, 2000);
    h.tick(6999);
    assert_eq!(h.state(), State::Active, "the grace period");
    h.tick(7000);
    assert_eq!(h.state(), State::Idle);
    {
        let w = h.world();
        assert!(w.session.is_none());
        assert_eq!((w.session_starts, w.session_stops), (1, 1));
    }
    assert_eq!(
        h.outputs(),
        (1, 1, Some(1)),
        "the writer and its output stay"
    );
    assert_eq!(h.counts(), counts(0, 0, 0, "ready"));

    // A link shorter than the debounce starts nothing.
    h.pw_consumers(1, 10_000);
    h.tick(10_200);
    h.pw_consumers(0, 10_200);
    h.tick(10_300);
    h.tick(20_000);
    assert_eq!(h.state(), State::Idle);
    assert_eq!(h.world().session_starts, 1);
}

#[test]
fn either_kind_of_consumer_keeps_the_session() {
    let mut h = H::new(PIPEWIRE, |_| {});
    h.pipewire_up(0);
    h.activate(0);
    h.pw_consumers(1, 500);
    assert_eq!(h.counts(), counts(2, 1, 1, "ready"));
    h.consumers(0, 1000);
    assert_eq!(h.counts(), counts(1, 0, 1, "ready"));
    h.tick(30_000);
    assert_eq!(h.state(), State::Active, "the PipeWire consumer stays");
    assert_eq!(h.world().session_starts, 1);
    // Back to the loopback alone, then nobody.
    h.consumers(2, 31_000);
    h.pw_consumers(0, 31_000);
    assert_eq!(h.counts(), counts(2, 2, 0, "ready"));
    h.tick(60_000);
    assert_eq!(h.state(), State::Active);
    h.consumers(0, 60_000);
    h.tick(65_000);
    assert_eq!(h.state(), State::Idle);
    assert_eq!(h.world().session_starts, 1);
}

#[test]
fn pipewire_events_of_an_output_that_does_not_run_are_ignored() {
    // A writer without the PipeWire output (no output ever started).
    let mut h = H::new("", |_| {});
    h.pw_state(PipeWireState::Streaming, 0);
    h.pw_consumers(1, 0);
    h.tick(1000);
    assert_eq!(h.state(), State::Idle);
    assert_eq!(h.world().session_starts, 0);
    assert_eq!(h.counts(), counts(0, 0, 0, "off"));
    assert_eq!(h.drain().0, []);

    // No writer at all (it failed and waits for its retry).
    let mut h = H::new(PIPEWIRE, |_| {});
    h.pipewire_up(0);
    h.pw_consumers(1, 0);
    {
        let mut w = h.world();
        w.writer_failure = Some("loopback writer failed: gone".into());
        w.sink = Err("no loopback".into());
    }
    h.tick(100);
    assert_eq!(h.state(), State::Error);
    assert_eq!(h.outputs(), (1, 1, None), "stopped with the writer");
    assert_eq!(h.counts(), counts(0, 0, 0, "off"), "reset with the writer");
    h.pw_consumers(1, 200);
    h.pw_state(PipeWireState::Streaming, 200);
    assert_eq!(h.counts(), counts(0, 0, 0, "off"));
    // The writer is back: a new output, connecting, with no link yet.
    h.world().sink = Ok(PathBuf::from("/dev/video11"));
    h.tick(2100);
    assert_eq!(h.state(), State::Idle);
    assert_eq!(h.outputs(), (2, 2, Some(2)));
    assert_eq!(h.counts(), counts(0, 0, 0, "connecting"));

    // After the output is disabled, its late events change nothing.
    let mut h = H::new(PIPEWIRE, |_| {});
    h.pipewire_up(0);
    h.reload("", 100).unwrap();
    assert_eq!(h.outputs(), (1, 1, None));
    assert_eq!(h.counts(), counts(0, 0, 0, "off"));
    h.pw_consumers(1, 200);
    h.pw_state(PipeWireState::Failed("late".into()), 200);
    h.tick(1000);
    assert_eq!(h.counts(), counts(0, 0, 0, "off"));
    assert_eq!(h.world().session_starts, 0);
}

#[test]
fn events_a_replaced_output_left_queued_are_ignored_whatever_their_order() {
    // No debounce: a stale link applied would open the camera at once.
    let config = |extra: &str| format!("[lifecycle]\nstart_debounce_ms = 0\n{extra}");
    let mut h = H::new(&config(PIPEWIRE), |_| {});
    h.pipewire_up(0);
    // Rebuilt for another output mode: a new output, generation 2.
    h.reload(&config(PIPEWIRE_720P), 100).unwrap();
    assert_eq!(h.outputs(), (2, 2, Some(2)));
    assert_eq!(h.counts(), counts(0, 0, 0, "connecting"));
    // The old output's link and state, still queued, arrive before the new
    // output has reported anything.
    h.pipewire_gen(1, PipeWireEvent::Consumers(1), 110);
    h.pipewire_gen(1, PipeWireEvent::State(PipeWireState::Streaming), 110);
    h.tick(110);
    assert_eq!(h.counts(), counts(0, 0, 0, "connecting"));
    assert_eq!(h.state(), State::Idle);
    assert_eq!(h.world().session_starts, 0);
    // The current output's events apply: its link starts the camera.
    h.pipewire_up(120);
    assert_eq!(h.counts(), counts(0, 0, 0, "ready"));
    h.pw_consumers(1, 130);
    assert_eq!(h.counts(), counts(1, 0, 1, "ready"));
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.world().session_starts, 1);

    // Disabled and enabled again (the writer untouched): the first
    // output's late link is no link of the second.
    let mut h = H::new(&config(PIPEWIRE), |_| {});
    h.pipewire_up(0);
    h.reload(&config(""), 100).unwrap();
    h.reload(&config(PIPEWIRE), 200).unwrap();
    assert_eq!(h.outputs(), (1, 2, Some(2)));
    h.pipewire_gen(1, PipeWireEvent::Consumers(1), 210);
    h.tick(210);
    assert_eq!(h.counts(), counts(0, 0, 0, "connecting"));
    assert_eq!(h.world().session_starts, 0);
    h.pipewire_gen(2, PipeWireEvent::Consumers(1), 220);
    assert_eq!(h.state(), State::Active);
}

#[test]
fn enabling_pipewire_on_reload_starts_it_at_once_and_leaves_the_writer() {
    // A loopback consumer streams meanwhile: neither it nor the session is
    // disturbed.
    let mut h = H::new("", |_| {});
    h.activate(0);
    h.drain();
    h.reload(PIPEWIRE, 1000).unwrap();
    assert_eq!(h.outputs(), (1, 1, Some(1)), "the writer is not rebuilt");
    assert_eq!(h.counts(), counts(1, 1, 0, "connecting"));
    assert_eq!(h.state(), State::Active);
    {
        let w = h.world();
        assert_eq!((w.session_starts, w.session_stops), (1, 0));
    }
    assert_eq!(
        h.drain().0,
        [Change::PipeWireOutput("connecting".into())],
        "only the PipeWire output changes"
    );
    h.pipewire_up(1100);
    h.pw_consumers(1, 1200);
    assert_eq!(h.counts(), counts(2, 1, 1, "ready"));

    // Without any consumer, the same.
    let mut h = H::new("", |_| {});
    h.reload(PIPEWIRE, 100).unwrap();
    assert_eq!(h.outputs(), (1, 1, Some(1)));
    assert_eq!(h.counts(), counts(0, 0, 0, "connecting"));
}

#[test]
fn disabling_pipewire_on_reload_waits_for_its_last_link_only() {
    // A PipeWire consumer streams: deferred, the output stays.
    let mut h = H::new(PIPEWIRE, |_| {});
    h.pipewire_up(0);
    h.pw_consumers(1, 0);
    h.tick(300);
    assert_eq!(h.state(), State::Active);
    h.reload("", 1000).unwrap();
    assert_eq!(
        h.outputs(),
        (1, 1, Some(1)),
        "a PipeWire consumer is linked"
    );
    assert_eq!(h.counts(), counts(1, 0, 1, "ready"));
    // A loopback consumer arrives meanwhile; the last link goes: the
    // output stops at once, the writer and the session stay.
    h.consumers(1, 1500);
    h.drain();
    h.pw_consumers(0, 2000);
    assert_eq!(h.outputs(), (1, 1, None));
    assert_eq!(h.world().pipewire_stops, 1);
    assert_eq!(h.counts(), counts(1, 1, 0, "off"));
    assert_eq!(h.state(), State::Active);
    {
        let w = h.world();
        assert_eq!((w.session_starts, w.session_stops), (1, 0));
    }
    assert_eq!(
        h.drain().0,
        [
            Change::Consumers(1),
            Change::PipeWireConsumers(0),
            Change::PipeWireOutput("off".into()),
        ]
    );

    // Only a loopback consumer: applied at once.
    let mut h = H::new(PIPEWIRE, |_| {});
    h.pipewire_up(0);
    h.activate(0);
    h.reload("", 1000).unwrap();
    assert_eq!(h.outputs(), (1, 1, None));
    assert_eq!(h.counts(), counts(1, 1, 0, "off"));
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.world().session_stops, 0);

    // Enabled again before the last link went: the output just stays.
    let mut h = H::new(PIPEWIRE, |_| {});
    h.pipewire_up(0);
    h.pw_consumers(1, 0);
    h.reload("", 100).unwrap();
    h.reload(PIPEWIRE, 200).unwrap();
    h.pw_consumers(0, 300);
    assert_eq!(h.outputs(), (1, 1, Some(1)));
    assert_eq!(h.counts(), counts(0, 0, 0, "ready"));
}

const PIPEWIRE_NO_NV12: &str = "[pipewire]\nenabled = true\noffer_nv12 = false\n";

#[test]
fn the_pipewire_output_starts_with_the_configured_nv12_offer() {
    let h = H::new(PIPEWIRE, |_| {});
    assert_eq!(h.world().pipewire_offer_nv12, Some(true), "the default");
    let h = H::new(PIPEWIRE_NO_NV12, |_| {});
    assert_eq!(h.world().pipewire_offer_nv12, Some(false));
    assert_eq!(h.outputs(), (1, 1, Some(1)));
}

#[test]
fn changing_the_nv12_offer_on_reload_rebuilds_the_pipewire_output_only() {
    // No link: rebuilt at once, the writer and the session stay.
    let mut h = H::new(PIPEWIRE, |_| {});
    h.pipewire_up(0);
    h.activate(0);
    h.drain();
    h.reload(PIPEWIRE_NO_NV12, 1000).unwrap();
    assert_eq!(h.outputs(), (1, 2, Some(2)), "a new output, same writer");
    assert_eq!(h.world().pipewire_offer_nv12, Some(false));
    assert_eq!(h.counts(), counts(1, 1, 0, "connecting"));
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.world().session_stops, 0);
    // The same config again: nothing is rebuilt.
    h.pipewire_up(1100);
    h.reload(PIPEWIRE_NO_NV12, 1200).unwrap();
    assert_eq!(h.outputs(), (1, 2, Some(2)));

    // A linked consumer defers it to the last link, as disabling does.
    let mut h = H::new(PIPEWIRE, |_| {});
    h.pipewire_up(0);
    h.pw_consumers(1, 0);
    h.reload(PIPEWIRE_NO_NV12, 1000).unwrap();
    assert_eq!(
        h.outputs(),
        (1, 1, Some(1)),
        "a PipeWire consumer is linked"
    );
    assert_eq!(h.world().pipewire_offer_nv12, Some(true));
    h.pw_consumers(0, 2000);
    assert_eq!(h.outputs(), (1, 2, Some(2)));
    assert_eq!(h.world().pipewire_offer_nv12, Some(false));
    assert_eq!(h.counts(), counts(0, 0, 0, "connecting"));

    // Changed back before the last link went: the output just stays.
    let mut h = H::new(PIPEWIRE, |_| {});
    h.pipewire_up(0);
    h.pw_consumers(1, 0);
    h.reload(PIPEWIRE_NO_NV12, 100).unwrap();
    h.reload(PIPEWIRE, 200).unwrap();
    h.pw_consumers(0, 300);
    assert_eq!(h.outputs(), (1, 1, Some(1)));

    // With the output disabled, the offer is only remembered for its start.
    let mut h = H::new("", |_| {});
    h.reload(PIPEWIRE_NO_NV12, 100).unwrap();
    assert_eq!(h.world().pipewire_offer_nv12, Some(false));
    assert_eq!(h.outputs(), (1, 1, Some(1)));
}

#[test]
fn an_output_mode_change_rebuilds_the_pipewire_output_too() {
    // Deferred while the PipeWire consumer streams, its grace included.
    let mut h = H::new(PIPEWIRE, |_| {});
    let before = h.world().writer.clone().map(|(_, c)| c);
    h.pipewire_up(0);
    h.pw_consumers(1, 0);
    h.tick(300);
    h.reload(PIPEWIRE_720P, 1000).unwrap();
    assert_eq!(h.outputs(), (1, 1, Some(1)));
    h.pw_consumers(0, 2000);
    h.tick(6999);
    assert_eq!(h.outputs(), (1, 1, Some(1)), "the grace period");
    h.tick(7000);
    assert_eq!(h.state(), State::Idle);
    assert_eq!(h.outputs(), (2, 2, Some(2)), "both rebuilt");
    assert_ne!(h.world().writer.clone().map(|(_, c)| c), before);
    assert_eq!(h.counts(), counts(0, 0, 0, "connecting"));

    // Deferred while a loopback consumer is present.
    let mut h = H::new(PIPEWIRE, |_| {});
    h.pipewire_up(0);
    h.consumers(1, 0);
    h.reload(PIPEWIRE_720P, 100).unwrap();
    assert_eq!(h.outputs(), (1, 1, Some(1)));
    h.consumers(0, 200);
    assert_eq!(h.outputs(), (2, 2, Some(2)));
    assert_eq!(h.world().session_starts, 0);

    // A mode change and PipeWire disabled with nobody: the writer alone
    // comes back.
    let mut h = H::new(PIPEWIRE, |_| {});
    h.pipewire_up(0);
    h.reload("[output]\nmode = \"1280x720@30\"\n", 100).unwrap();
    assert_eq!(h.outputs(), (2, 1, None));
    assert_eq!(h.counts(), counts(0, 0, 0, "off"));
}

#[test]
fn a_failed_or_unsupported_pipewire_output_is_no_fault() {
    let mut h = H::new(PIPEWIRE, |_| {});
    h.pipewire_up(0);
    h.activate(0);
    h.pw_consumers(1, 400);
    h.drain();
    h.pw_state(
        PipeWireState::Failed("the PipeWire daemon went away".into()),
        500,
    );
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.snap().error, "");
    // A failed output has no links; the loopback consumer stays.
    assert_eq!(h.counts(), counts(1, 1, 0, "failed"));
    assert_eq!(
        h.drain().0,
        [
            Change::Consumers(1),
            Change::PipeWireConsumers(0),
            Change::PipeWireOutput("failed".into()),
        ]
    );
    // The same failure again changes nothing.
    h.pw_state(
        PipeWireState::Failed("the PipeWire daemon went away".into()),
        600,
    );
    assert_eq!(h.drain().0, []);
    h.consumers(0, 1000);
    h.tick(6000);
    assert_eq!(h.state(), State::Idle);
    assert_eq!(h.snap().error, "");

    // A build without PipeWire support: idle, no error, the loopback works.
    let mut h = H::new(PIPEWIRE, |_| {});
    h.pw_state(PipeWireState::Unsupported, 0);
    h.pw_consumers(0, 0);
    assert_eq!(h.state(), State::Idle);
    assert_eq!(h.snap().error, "");
    assert_eq!(h.counts(), counts(0, 0, 0, "unsupported"));
    h.activate(100);
    assert_eq!(h.snap().error, "");
    // An error of the loopback side is still reported as before.
    h.world().failure = Some(Failure::Output("output pipeline failed: boom".into()));
    h.tick(1000);
    assert_eq!(h.state(), State::Error);
    assert_eq!(h.snap().error, "output pipeline failed: boom");
    assert_eq!(h.snap().pipewire_output, "unsupported");
}

#[test]
fn the_pipewire_states_have_their_wire_names() {
    assert_eq!(
        [
            PipeWireState::Off,
            PipeWireState::Unsupported,
            PipeWireState::Connecting,
            PipeWireState::Ready,
            PipeWireState::Streaming,
            PipeWireState::Failed("x".into()),
        ]
        .map(|s| s.as_str()),
        [
            "off",
            "unsupported",
            "connecting",
            "ready",
            "streaming",
            "failed"
        ]
    );
    assert_eq!(obc_ipc::PIPEWIRE_OFF, PipeWireState::Off.as_str());
}

// ---- the GPU parked between sessions (backend::park) ----

impl H {
    /// Active from `ms`, then the consumer leaves: idle after the grace.
    fn activate_then_idle(&mut self, ms: u64) {
        self.activate(ms);
        self.consumers(0, ms + 1000);
        self.tick(ms + 1000 + 5000);
        assert_eq!(self.state(), State::Idle, "{:?}", self.snap());
    }
}

#[test]
fn idle_after_a_session_parks_the_gpu() {
    let mut h = H::new("", |_| {});
    assert!(!h.world().parked, "nothing to park before a session");
    h.activate_then_idle(0);
    let w = h.world();
    assert!(w.parked);
    assert_eq!((w.parks, w.unparks), (1, 0));
    assert_eq!(w.gpu_log, ["build", "park"]);
    assert!(
        w.writer.is_some(),
        "the writer keeps the placeholder flowing"
    );
    // The backend's parker is the daemon's own state machine.
    assert!(
        w.parker_said
            .iter()
            .any(|(l, m)| *l == Level::Info && m.starts_with("GPU parked in")),
        "{:?}",
        w.parker_said
    );
}

#[test]
fn a_failed_first_start_has_nothing_to_park() {
    let mut h = H::new("", |w| {
        w.fail_start = Some("output pipeline: no cudaupload".into())
    });
    h.consumers(1, 0);
    h.tick(300);
    assert_eq!(h.state(), State::Error, "{:?}", h.snap());
    let w = h.world();
    assert!(!w.parked, "CUDA never initialised: nothing parked");
    assert_eq!((w.parks, w.unparks, w.park_failures), (0, 0, 0));
    assert!(w.gpu_log.is_empty(), "{:?}", w.gpu_log);
    assert!(
        w.parker_said.iter().all(|(l, _)| *l == Level::Debug),
        "{:?}",
        w.parker_said
    );
}

#[test]
fn a_start_from_idle_unparks_before_the_session_is_built() {
    let mut h = H::new("", |_| {});
    h.activate_then_idle(0);
    h.activate(10_000);
    {
        let w = h.world();
        assert!(!w.parked);
        assert_eq!(w.gpu_log, ["build", "park", "unpark", "build"]);
        assert_eq!(w.session_starts, 2);
    }
    // Three cycles: one park and one unpark each.
    h.consumers(0, 11_000);
    h.tick(16_000);
    h.activate_then_idle(20_000);
    let w = h.world();
    assert_eq!((w.parks, w.unparks), (3, 2));
    assert!(w.parked);
}

#[test]
fn an_unpark_failure_is_an_error_with_the_writer_kept_and_a_later_start_retries() {
    let mut h = H::new("", |_| {});
    h.activate_then_idle(0);
    h.world().fail_unpark = Some("cuCheckpointProcessRestore failed".into());
    h.consumers(1, 10_000);
    h.tick(10_300);
    assert_eq!(h.state(), State::Error, "{:?}", h.snap());
    let error = h.snap().error;
    assert!(
        error.starts_with("the GPU could not be restored from its parked state")
            && error.contains("cuCheckpointProcessRestore failed")
            && error.ends_with("restart the daemon"),
        "{error}"
    );
    {
        let w = h.world();
        assert!(w.session.is_none(), "no session");
        assert_eq!(
            w.ingests,
            ["usb-a"],
            "no camera opened since the first session"
        );
        assert!(w.parked, "still parked");
        assert_eq!(w.gpu_log, ["build", "park"], "nothing built while parked");
        // Fail closed: the writer keeps sending its black placeholders.
        assert!(w.writer.is_some());
        assert_eq!(w.writer_starts, 1);
    }
    // No retry by itself.
    h.tick(20_000);
    assert_eq!(h.world().session_starts, 1);
    // Cleared, a control call retries (the consumer is still there).
    h.world().fail_unpark = None;
    h.call(|r| Command::SetEnabled(true, r), 21_000).unwrap();
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.snap().error, "");
    let w = h.world();
    assert!(!w.parked);
    assert_eq!(w.gpu_log, ["build", "park", "unpark", "build"]);
}

#[test]
fn off_after_a_session_stays_parked() {
    let mut h = H::new("", |_| {});
    h.activate(0);
    h.call(|r| Command::SetEnabled(false, r), 1000).unwrap();
    assert_eq!(h.state(), State::Off);
    // Off with the consumer still there, for a while.
    h.tick(30_000);
    let w = h.world();
    assert!(w.parked);
    assert_eq!((w.parks, w.unparks), (1, 0));
    assert!(w.writer.is_some());
}

#[test]
fn a_park_failure_leaves_the_gpu_awake_and_the_daemon_working() {
    let mut h = H::new("", |w| w.fail_park = Some("lock timed out".into()));
    h.activate_then_idle(0);
    {
        let w = h.world();
        assert!(!w.parked);
        assert_eq!((w.parks, w.park_failures), (0, 1));
        assert!(
            w.parker_said.iter().any(|(l, m)| *l == Level::Warning
                && m.contains("stays awake")
                && m.contains("lock timed out")),
            "{:?}",
            w.parker_said
        );
    }
    assert_eq!(h.snap().error, "", "not an error: the GPU only stays awake");
    h.activate(10_000);
    let w = h.world();
    assert_eq!(w.unparks, 0, "nothing to unpark");
    assert_eq!(w.session_starts, 2);
}

#[test]
fn a_failed_start_parks_the_gpu_again() {
    let mut h = H::new("", |_| {});
    h.activate_then_idle(0);
    h.world().fail_start = Some("output pipeline: no cudaupload".into());
    h.consumers(1, 10_000);
    h.tick(10_300);
    assert_eq!(h.state(), State::Error);
    let w = h.world();
    assert!(w.parked, "parked again after the failed start");
    assert_eq!(w.gpu_log, ["build", "park", "unpark", "park"]);
}

#[test]
fn a_session_that_did_not_stop_cleanly_turns_parking_off_until_the_daemon_restarts() {
    let mut h = H::new("", |_| {});
    // A first clean cycle parks.
    h.activate_then_idle(0);
    assert_eq!(h.world().parks, 1);
    // The next session's stop is unclean: not parked, and said once.
    h.world().fail_stop = Some("output pipeline: ended in Paused, not Null".into());
    h.activate(10_000);
    h.consumers(0, 11_000);
    h.tick(16_000);
    assert_eq!(h.state(), State::Idle, "{:?}", h.snap());
    assert_eq!(h.snap().error, "", "not an error: the GPU only stays awake");
    {
        let w = h.world();
        assert!(!w.parked);
        assert_eq!(
            w.gpu_log,
            ["build", "park", "unpark", "build"],
            "not parked"
        );
        let disabled: Vec<_> = w
            .parker_said
            .iter()
            .filter(|(l, m)| {
                *l == Level::Warning && m.contains("disabled until the daemon restarts")
            })
            .collect();
        assert_eq!(disabled.len(), 1, "{:?}", w.parker_said);
        assert!(
            disabled[0].1.contains("ended in Paused, not Null"),
            "{}",
            disabled[0].1
        );
    }
    assert!(h.fake.parking_tainted());
    // The next sessions still run, and a clean stop parks nothing either.
    for at in [20_000, 40_000] {
        h.activate_then_idle(at);
    }
    let w = h.world();
    assert_eq!(w.session_starts, 4);
    assert_eq!((w.session_stops, w.parks, w.unparks), (4, 1, 1));
    assert!(!w.parked, "sticky: never parked again");
    assert_eq!(
        w.parker_said
            .iter()
            .filter(|(l, _)| *l == Level::Warning)
            .count(),
        1,
        "{:?}",
        w.parker_said
    );
}

#[test]
fn a_failed_start_whose_cleanup_was_unclean_does_not_park_again() {
    let mut h = H::new("", |_| {});
    h.activate_then_idle(0);
    {
        let mut w = h.world();
        w.fail_start = Some("output pipeline: state change failed".into());
        w.fail_start_cleanup = Some("output pipeline: ended in Paused, not Null".into());
    }
    h.consumers(1, 10_000);
    h.tick(10_300);
    assert_eq!(h.state(), State::Error);
    assert!(
        h.snap().error.contains("state change failed"),
        "the start's error: {}",
        h.snap().error
    );
    {
        let w = h.world();
        assert!(!w.parked, "not parked again");
        assert_eq!(w.gpu_log, ["build", "park", "unpark"]);
    }
    assert!(h.fake.parking_tainted());
    // Cleared, the next start runs; its clean stop parks nothing.
    {
        let mut w = h.world();
        w.fail_start = None;
        w.fail_start_cleanup = None;
    }
    h.call(|r| Command::SetEnabled(true, r), 11_000).unwrap();
    assert_eq!(h.state(), State::Active);
    h.consumers(0, 12_000);
    h.tick(17_000);
    assert_eq!(h.state(), State::Idle);
    let w = h.world();
    assert_eq!(w.gpu_log, ["build", "park", "unpark", "build"]);
    assert!(!w.parked);
}

#[test]
fn a_camera_switch_that_left_a_pipeline_outside_null_turns_parking_off() {
    let mut h = H::new("", |w| {
        w.fail_ingest = vec!["usb-b".into()];
        w.unclean_switch = Some("stopping the previous ingest: ended in Ready, not Null".into());
    });
    h.activate(0);
    assert!(
        h.call(|r| Command::SelectCamera("usb-b".into(), r), 1000)
            .is_err()
    );
    assert_eq!(h.snap().active_camera, "usb-a", "fell back");
    assert!(
        !h.fake.parking_tainted(),
        "the session runs on: nothing to park yet"
    );
    // Its stop is clean, but the switch's was not: not parked.
    h.consumers(0, 2000);
    h.tick(7000);
    assert_eq!(h.state(), State::Idle, "{:?}", h.snap());
    assert!(h.fake.parking_tainted());
    {
        let w = h.world();
        assert!(!w.parked);
        assert_eq!(w.parks, 0);
        let warned: Vec<_> = w
            .parker_said
            .iter()
            .filter(|(l, _)| *l == Level::Warning)
            .collect();
        assert_eq!(warned.len(), 1, "{:?}", w.parker_said);
        assert!(
            warned[0].1.contains("switching to Beta Cam")
                && warned[0].1.contains("ended in Ready, not Null")
                && warned[0].1.contains("disabled until the daemon restarts"),
            "{}",
            warned[0].1
        );
    }
    // Sticky: a later session without a switch parks nothing either.
    h.activate_then_idle(10_000);
    assert_eq!(h.world().parks, 0);
}

#[test]
fn a_session_stop_that_failed_with_every_pipeline_at_null_still_parks() {
    // A camera unplugged mid-call with fixed_framerate: its control cannot
    // be restored, but nothing of the session is left.
    let mut h = H::new("", |_| {});
    h.world().fail_stop_at_null = Some(
        "camera control on /dev/video0: restoring exposure_dynamic_framerate: No such device"
            .into(),
    );
    h.activate_then_idle(0);
    assert!(!h.fake.parking_tainted());
    let w = h.world();
    assert!(w.parked);
    assert_eq!(w.parks, 1);
    let warned: Vec<_> = w
        .parker_said
        .iter()
        .filter(|(l, _)| *l == Level::Warning)
        .collect();
    assert_eq!(warned.len(), 1, "{:?}", w.parker_said);
    assert!(
        warned[0].1.contains("No such device") && !warned[0].1.contains("disabled"),
        "{}",
        warned[0].1
    );
}
