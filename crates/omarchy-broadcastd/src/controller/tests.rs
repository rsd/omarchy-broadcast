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

/// The tracking of the stages of revision 8, as the tests read it.
impl<B: MediaBackend> Controller<B> {
    fn track_of(&self, id: &str) -> StageTrack {
        self.stages
            .iter()
            .find(|(s, _)| s == id)
            .map(|(_, t)| t.clone())
            .unwrap_or_default()
    }
    fn denoise_failure(&self) -> Option<String> {
        self.track_of("denoise").failure
    }
    fn relight_failure(&self) -> Option<String> {
        self.track_of("relight").failure
    }
    fn denoise_seen(&self) -> Option<EffectStatus> {
        self.track_of("denoise").seen
    }
    fn poll_denoise(&mut self, status: EffectStatus, effect: &EffectStatus) {
        self.poll_stage(&StageId::DENOISE, status, effect);
    }
    fn poll_relight(&mut self, status: EffectStatus, effect: &EffectStatus) {
        self.poll_stage(&StageId::RELIGHT, status, effect);
    }
}

fn denoise_failure_text(reason: &str) -> String {
    stage_failure_text(&StageId::DENOISE, "noise removal", reason)
}

fn relight_failure_text(reason: &str) -> String {
    stage_failure_text(&StageId::RELIGHT, "relighting", reason)
}

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
    /// The `SettingsChanged` notes drained: (revision, serial, changes).
    settings_notes: Vec<(u32, u32, Vec<SettingChange>)>,
    /// The `ConfigChanged` notes drained: their serials.
    config_notes: Vec<u32>,
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
            cache: Some(dir.path().join("cache")),
            runtime: Some(dir.path().join("run")),
            ledger: None,
        };
        fs::create_dir(dir.path().join("run")).unwrap();
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
            settings_notes: Vec::new(),
            config_notes: Vec::new(),
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
        {
            // The devices outlive the daemon, controls and all.
            let old = self.fake.world();
            let mut w = fake.world();
            w.cameras = old.cameras.clone();
            w.controls = old.controls.clone();
        }
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
                Note::PreviewFrame(_) => {}
                Note::SettingsChanged {
                    revision,
                    serial,
                    changes,
                } => self.settings_notes.push((revision, serial, changes)),
                Note::ConfigChanged(serial) => self.config_notes.push(serial),
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

    /// The managed file `config.d/50-panel.toml` (empty when absent).
    fn managed(&self) -> toml::Table {
        match fs::read_to_string(obc_core::config_d::managed_path(&self.paths.config)) {
            Ok(text) => text.parse().unwrap(),
            Err(_) => toml::Table::new(),
        }
    }

    /// The value of the dotted `key` in the managed file.
    fn managed_key(&self, key: &str) -> Option<toml::Value> {
        obc_core::config_d::table_get(&self.managed(), key).cloned()
    }

    /// The state file, once a settings change waiting for its delayed
    /// save is saved.
    fn persisted(&mut self) -> RuntimeState {
        self.ctrl.flush_settings();
        let (s, warn) = RuntimeState::load(&self.paths.state);
        assert_eq!(warn, None);
        legacy_view(s)
    }
}

/// A state file's module settings as the typed keys of revision 8 read
/// them, for the assertions written against those.
fn legacy_view(mut s: RuntimeState) -> RuntimeState {
    use obc_core::modules::StoredValue as V;
    use obc_core::modules::legacy as l;
    let get = |id: &str| s.settings.get(id).cloned();
    let b = |v: Option<V>| match v {
        Some(V::Bool(b)) => Some(b),
        _ => None,
    };
    let i = |v: Option<V>| match v {
        Some(V::Int(i)) => Some(i),
        _ => None,
    };
    s.denoise = s.denoise.or(b(get(l::DENOISE)));
    s.enhance = s.enhance.or(b(get(l::ENHANCE)));
    s.relight = s.relight.or(b(get(l::RELIGHT)));
    s.relight_light = s.relight_light.or(match get(l::LIGHT) {
        Some(V::Str(x)) => x.parse().ok(),
        _ => None,
    });
    s.relight_direction = s
        .relight_direction
        .or(i(get(l::DIRECTION)).map(|v| v as i32));
    s.relight_brightness = s
        .relight_brightness
        .or(i(get(l::BRIGHTNESS)).map(|v| v as u32));
    s
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
        cache: None,
        runtime: None,
        ledger: None,
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
fn loading_the_modules_moves_the_serial_the_empty_list_had() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        config: dir.path().join("none.toml"),
        state: dir.path().join("state").join("state.toml"),
        cache: None,
        runtime: None,
        ledger: None,
    };
    let fake = FakeBackend::default();
    fake.world().cameras = vec![alpha()];
    let (published, mut notes) = Published::new(preliminary_snapshot(&paths));
    let mut ctrl =
        Controller::new_without_modules(fake.clone(), paths, published, None, Instant::now());
    let empty = ctrl.snapshot().modules_serial.unwrap();
    while notes.try_recv().is_ok() {}
    ctrl.load_modules(Instant::now());
    let loaded = ctrl.snapshot().modules_serial.unwrap();
    assert_ne!(loaded, empty, "the list that arrives is a change");
    let mut announced = false;
    while let Ok(n) = notes.try_recv() {
        if let Note::Changed(c) = n {
            announced |= c.contains(&Change::ModulesSerial(loaded));
        }
    }
    assert!(announced, "the change is announced");
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
    // What `GetConfigFiles` shows changed: `ConfigSerial` moves.
    assert_eq!(
        h.drain().0,
        [Change::ConfigSerial(2), Change::Error(msg.clone())]
    );

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
            Change::Warning(w) => format!("warning={w}"),
            Change::OutputMode(m) => format!("mode={m}"),
            Change::Effect(e) => format!("effect={e}"),
            Change::BlurStrength(v) => format!("strength={v}"),
            Change::BackgroundImage(p) => format!("image={p}"),
            Change::Denoise(b) => format!("denoise={b}"),
            Change::LoopbackConsumers(n) => format!("loopback={n}"),
            Change::PipeWireConsumers(n) => format!("pipewire={n}"),
            Change::PreviewConsumers(n) => format!("preview={n}"),
            Change::PipeWireOutput(s) => format!("pipewire_output={s}"),
            Change::CameraControls(v) => format!("controls={}:{}", v.camera, v.controls.len()),
            Change::SettingsRevision(n) => format!("settings_revision={n}"),
            Change::SettingsSchemaSerial(n) => format!("settings_serial={n}"),
            Change::ModulesSerial(n) => format!("modules_serial={n}"),
            Change::BackendProfile(p) => format!("profile={p}"),
            Change::ConfigSerial(n) => format!("config_serial={n}"),
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
            "controls=:0",
            "settings_serial=2",
            "settings_revision=2",
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
        cache: None,
        runtime: None,
        ledger: None,
    });
    let mut b = a.clone();
    b.state = State::Active;
    b.consumers = 2;
    b.active_camera = "x".into();
    b.loopback_consumers = 1;
    b.pipewire_consumers = 1;
    b.pipewire_output = "streaming".into();
    b.denoise = true;
    assert_eq!(
        diff(&a, &b),
        [
            Change::ActiveCamera("x".into()),
            Change::Denoise(true),
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
    // The config's image, a file that can be read.
    let images = tempfile::tempdir().unwrap();
    let cfg_image = images.path().join("office.png");
    fs::write(&cfg_image, b"an image").unwrap();
    let cfg_image = cfg_image.display().to_string();
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
fn an_effect_warning_with_none_ends_with_the_session() {
    let config = "[effect]\nmode = \"blur\"\non_failure = \"none\"\n\
                  [lifecycle]\nidle_grace_s = 0.0\nstart_debounce_ms = 0\n";
    let mut h = H::new(config, |_| {});
    h.activate(0);
    h.world().effect_status = EffectStatus::Failed("processing a frame failed".into());
    h.tick(1000);
    assert_eq!(h.state(), State::Active);
    let warning = shown_without("processing a frame failed");
    assert_eq!(h.snap().warning, warning);
    assert_eq!(h.snap().error, "");
    // The consumer leaves: idle, and the effect's warning is gone with it.
    h.consumers(0, 2000);
    h.tick(3000);
    assert_eq!(h.state(), State::Idle);
    assert_eq!(
        (h.snap().error, h.snap().warning),
        (String::new(), String::new())
    );
    // Off ends it too.
    h.activate(4000);
    h.world().effect_status = EffectStatus::Failed("processing a frame failed".into());
    h.tick(5000);
    assert_eq!(h.snap().warning, warning);
    h.call(|r| Command::SetEnabled(false, r), 6000).unwrap();
    assert_eq!(h.state(), State::Off);
    assert_eq!(
        (h.snap().error, h.snap().warning),
        (String::new(), String::new())
    );
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

/// `Warning` of a selected `blur` that cannot run for `reason`, with
/// `on_failure = "black"`.
fn black(reason: &str) -> String {
    format!("effect blur unavailable: {reason}; the video is black until it can run")
}

/// `Warning` of a selected `blur` that cannot run for `reason`, with
/// `on_failure = "none"`.
fn shown_without(reason: &str) -> String {
    format!(
        "effect blur unavailable: {reason}; the camera image is shown without the effect \
         (effect.on_failure = \"none\")"
    )
}

#[test]
fn an_unavailable_effect_with_black_streams_black_with_a_warning_and_a_call_retries() {
    let mut h = H::new("[effect]\nmode = \"blur\"\n", |w| {
        w.effect_unavailable = Some("libVideoFX.so not found".into())
    });
    h.activate(0);
    let want = black("libVideoFX.so not found");
    let snap = h.snap();
    assert_eq!((snap.error.as_str(), snap.warning.as_str()), ("", &*want));
    {
        let w = h.world();
        assert_eq!(w.session_starts, 1, "the output was built");
        assert_eq!(w.ingests.len(), 1, "the camera was opened");
        assert_eq!(w.effect_probes, 1);
        // The element is given the effect, with `black`: its load fails,
        // and its frames are black (the room hidden).
        let e = w.effect.as_ref().unwrap();
        assert_eq!(
            (e.mode, e.on_failure),
            (EffectMode::Blur, EffectOnFailure::Black)
        );
        assert!(matches!(w.effect_status, EffectStatus::Failed(_)));
    }
    // No retry by itself, no spinning, and no error.
    h.tick(10_000);
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.world().effect_probes, 1);
    assert_eq!(h.world().session_starts, 1);
    // A control call retries: still unavailable, still streaming.
    h.set_effect("blur", 11_000).unwrap();
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.snap().warning, want);
    // Once it can run, the next call gives it, in the same session.
    h.world().effect_unavailable = None;
    h.set_effect("blur", 12_000).unwrap();
    let snap = h.snap();
    assert_eq!((snap.error.as_str(), snap.warning.as_str()), ("", ""));
    assert_eq!(h.world().effect_status, EffectStatus::Ready);
    assert_eq!(h.world().session_starts, 1);
    // Selecting no effect clears it too.
    h.world().effect_unavailable = Some("libVideoFX.so not found".into());
    h.set_effect("none", 12_500).unwrap();
    h.set_effect("blur", 13_000).unwrap();
    assert_eq!(h.snap().warning, want);
    h.set_effect("none", 14_000).unwrap();
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.snap().warning, "");
    assert_eq!(h.applied(), Some(EffectMode::None));
}

#[test]
fn settings_the_output_cannot_be_built_with_are_still_an_error_with_black() {
    // `replace` with an image that cannot be read: not a module's failure,
    // and the output cannot be built with it.
    let mut h = H::new(
        "[effect]\nmode = \"replace\"\nbackground_image = \"/nonexistent.png\"\n",
        |_| {},
    );
    h.activate_expecting_error();
    assert!(
        h.snap().error.starts_with("effect replace unavailable: "),
        "{}",
        h.snap().error
    );
    assert_eq!(h.snap().warning, "");
    assert_eq!(h.world().session_starts, 0, "no output was built");
    assert!(h.world().ingests.is_empty(), "no camera was opened");
}

#[test]
fn an_unavailable_effect_with_none_streams_without_it_and_says_why() {
    let mut h = H::new("[effect]\nmode = \"blur\"\non_failure = \"none\"\n", |w| {
        w.effect_unavailable = Some("libVideoFX.so not found".into())
    });
    h.activate(0);
    assert_eq!(h.applied(), Some(EffectMode::None), "started without it");
    let snap = h.snap();
    assert_eq!(snap.warning, shown_without("libVideoFX.so not found"));
    assert_eq!(snap.error, "");
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.snap().effect, "blur", "the selection is still blur");
    h.tick(2000);
    assert_eq!(h.snap().warning, snap.warning, "it stays while it streams");
    // A replace with no image cannot run either: the same rule.
    let mut h = H::new("[effect]\non_failure = \"none\"\n", |_| {});
    h.runtime_effect(EffectMode::Replace);
    h.activate(0);
    assert!(
        h.snap().warning.contains("needs a background image"),
        "{}",
        h.snap().warning
    );
    assert_eq!(h.snap().error, "");
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
fn an_effect_failure_with_black_streams_black_with_a_warning_and_a_call_retries() {
    let mut h = H::new("[effect]\nmode = \"blur\"\n", |w| w.effect_slow = true);
    h.activate(0);
    assert_eq!(h.world().effect_status, EffectStatus::Loading);
    h.world().effect_status = EffectStatus::Ready;
    h.tick(500);
    h.drain();
    // The effect fails while streaming (a frame, the GPU): the element's
    // frames are black from then on, the session runs on.
    h.world().effect_status = EffectStatus::Failed("processing a frame failed".into());
    h.tick(1000);
    assert_eq!(h.state(), State::Active);
    let want = black("processing a frame failed");
    let snap = h.snap();
    assert_eq!((snap.error.as_str(), snap.warning.as_str()), ("", &*want));
    {
        let w = h.world();
        assert!(w.session.is_some(), "the camera stays open");
        assert_eq!(w.session_stops, 0);
        let e = w.effect.as_ref().unwrap();
        assert_eq!(
            (e.mode, e.on_failure),
            (EffectMode::Blur, EffectOnFailure::Black)
        );
    }
    let (changes, _) = h.drain();
    assert_eq!(states_in(&changes), [], "no state change");
    assert!(changes.contains(&Change::Warning(want.clone())));
    assert!(!changes.iter().any(|c| matches!(c, Change::Error(_))));
    // Nothing restarts by itself while the consumer stays.
    h.tick(20_000);
    assert_eq!(h.world().session_starts, 1);
    assert_eq!(h.snap().warning, want);
    // A call for the effect retries: it runs again, in the same session.
    h.world().effect_slow = false;
    h.set_effect("blur", 22_000).unwrap();
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.world().session_starts, 1);
    let snap = h.snap();
    assert_eq!((snap.error.as_str(), snap.warning.as_str()), ("", ""));
    // A failure at load, at start, is the same, in the same step.
    let mut h = H::new("[effect]\nmode = \"blur\"\n", |w| {
        w.effect_fail = Some("the model did not load".into())
    });
    h.activate(0);
    let snap = h.snap();
    let want = black("the model did not load");
    assert_eq!((snap.error.as_str(), snap.warning.as_str()), ("", &*want));
    assert_eq!(h.world().ingests.len(), 1, "the camera is open");
    // A reload retries it too: once it loads, the warning clears.
    h.world().effect_fail = None;
    h.reload("[effect]\nmode = \"blur\"\n", 1000).unwrap();
    assert_eq!(h.world().effect_status, EffectStatus::Ready);
    assert_eq!(h.snap().warning, "");
    assert_eq!(h.world().session_starts, 1);
    // The consumer leaves with the effect failing: the warning ends with
    // the session, and the next one tries again.
    h.world().effect_fail = Some("the model did not load".into());
    h.set_effect("none", 1500).unwrap();
    h.set_effect("blur", 2000).unwrap();
    assert_eq!(h.snap().warning, want);
    h.consumers(0, 3000);
    h.tick(70_000);
    assert_eq!(h.state(), State::Idle);
    assert_eq!(h.snap().warning, "");
    h.world().effect_fail = None;
    h.activate(71_000);
    let snap = h.snap();
    assert_eq!((snap.error.as_str(), snap.warning.as_str()), ("", ""));
}

#[test]
fn an_effect_failure_with_none_keeps_streaming_with_the_reason_in_the_warning() {
    let mut h = H::new("[effect]\nmode = \"blur\"\non_failure = \"none\"\n", |_| {});
    h.activate(0);
    h.drain();
    h.world().effect_status = EffectStatus::Failed("processing a frame failed".into());
    h.tick(1000);
    assert_eq!(h.state(), State::Active);
    assert!(h.world().session.is_some());
    let w = shown_without("processing a frame failed");
    assert_eq!(h.snap().warning, w);
    assert_eq!(h.snap().error, "");
    // Reported once, not on every poll.
    h.tick(1250);
    h.tick(1500);
    let warnings = h
        .drain()
        .0
        .into_iter()
        .filter(|c| matches!(c, Change::Warning(_)))
        .count();
    assert_eq!(warnings, 1);
    // A retry that fails the same way is reported again.
    h.world().effect_fail = Some("processing a frame failed".into());
    h.set_effect("blur", 1800).unwrap();
    assert_eq!(h.snap().warning, w, "the same failure, reported again");
    h.world().effect_fail = None;
    // A call retries the effect on the running session; ready clears it.
    h.set_effect("blur", 2000).unwrap();
    assert_eq!(h.snap().warning, "");
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
    assert!(h.snap().warning.contains("unreadable"));
    assert_eq!(h.snap().error, "");
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

// ---- noise removal (the denoise stage of the effect element) ----

impl H {
    fn set_denoise(&mut self, on: bool, ms: u64) -> Result<(), IpcError> {
        self.call(|r| Command::SetDenoise(on, r), ms)
    }

    /// Whether the running session was given noise removal.
    fn denoise_applied(&self) -> Option<bool> {
        self.world().effect.as_ref().map(|e| e.denoise())
    }
}

const NO_DENOISE: &str = "NvVFX_Load failed (-2)";

fn denoise_error(reason: &str) -> String {
    format!("noise removal unavailable: {reason}; the camera image is shown without it")
}

#[test]
fn set_denoise_persists_publishes_applies_and_survives_a_restart() {
    let mut h = H::new("", |_| {});
    assert!(!h.snap().denoise);
    h.set_denoise(true, 0).unwrap();
    assert!(h.snap().denoise);
    let changes = h.drain().0;
    assert!(changes.contains(&Change::Denoise(true)), "{changes:?}");
    assert_eq!(Change::Denoise(true).name(), "Denoise");
    assert_eq!(h.persisted().denoise, Some(true));
    assert_eq!(h.persisted().effect, None, "the other keys stay unset");
    // A restart serves it, before and after the controller runs.
    assert!(preliminary_snapshot(&h.paths).denoise);
    h.restart();
    assert!(h.snap().denoise);
    // The session starts with it, the effect `none`.
    h.activate(1000);
    assert_eq!(h.applied(), Some(EffectMode::None));
    assert_eq!(h.denoise_applied(), Some(true));
    h.tick(1500);
    assert_eq!(h.ctrl.denoise_seen(), Some(EffectStatus::Ready));
    // Off while streaming: applied at once, no restart.
    h.set_denoise(false, 2000).unwrap();
    assert_eq!(h.denoise_applied(), Some(false));
    assert_eq!(h.world().denoise_status, EffectStatus::Off);
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.world().session_starts, 1);
    assert_eq!(h.persisted().denoise, Some(false));
    // Stacked with an effect: switching it while streaming keeps it.
    h.set_denoise(true, 2500).unwrap();
    h.set_effect("blur", 3000).unwrap();
    let applied = h.world().effect.clone().unwrap();
    assert_eq!((applied.mode, applied.denoise()), (EffectMode::Blur, true));
    assert_eq!(h.world().session_starts, 1);
    assert_eq!(h.snap().error, "");
}

#[test]
fn the_state_files_denoise_takes_precedence_over_the_config() {
    let mut h = H::new("[effect]\ndenoise = true\n", |_| {});
    assert!(h.snap().denoise);
    assert_eq!(h.persisted().denoise, None, "nothing written at start");
    h.set_denoise(false, 0).unwrap();
    assert!(!h.snap().denoise);
    h.reload("[effect]\ndenoise = true\n", 100).unwrap();
    assert!(!h.snap().denoise, "the state file's value still applies");
    h.restart();
    assert!(!h.snap().denoise);
    h.activate(1000);
    assert_eq!(h.denoise_applied(), Some(false));
}

#[test]
fn a_repeated_set_denoise_neither_rewrites_the_state_nor_reloads_the_session() {
    let mut h = H::new("[effect]\nmode = \"blur\"\n", |_| {});
    h.set_denoise(true, 0).unwrap();
    h.activate(0);
    h.tick(500);
    assert_eq!(h.ctrl.denoise_seen(), Some(EffectStatus::Ready));
    let sets = h.world().effect_sets.len();
    // The last good contract, kept at the first frame, saved first.
    h.ctrl.flush_settings();
    fs::remove_file(&h.paths.state).unwrap();
    h.set_denoise(true, 1000).unwrap();
    h.tick(1500);
    assert!(!h.paths.state.exists(), "the state file was rewritten");
    assert_eq!(h.world().effect_sets.len(), sets);
    assert!(effect_changes(&h.drain().0).is_empty());
    // A change of the denoise request is followed; the effect's load is
    // not reported again.
    h.set_denoise(false, 2000).unwrap();
    assert_eq!(h.world().effect_sets.len(), sets + 1);
    assert_eq!(h.ctrl.effect_seen, Some(EffectStatus::Ready));
    assert_eq!(h.ctrl.denoise_seen(), Some(EffectStatus::Off));
}

#[test]
fn set_denoise_clears_a_fault_and_the_session_starts_again() {
    let mut h = H::new("[effect]\nmode = \"blur\"\n", |_| {});
    h.activate(0);
    h.world().failure = Some(Failure::Output("output pipeline failed: boom".into()));
    h.tick(1000);
    assert_eq!(h.state(), State::Error);
    assert!(h.world().session.is_none());
    h.set_denoise(true, 2000).unwrap();
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.world().session_starts, 2);
    assert_eq!(h.snap().error, "");
    assert_eq!(h.denoise_applied(), Some(true));
}

#[test]
fn a_denoise_failure_keeps_streaming_whatever_on_failure_says() {
    for on_failure in ["black", "none"] {
        // `none` with denoise starts without any capability check, even
        // with the effect backend missing: the failure arrives by the poll.
        let config = format!("[effect]\ndenoise = true\non_failure = \"{on_failure}\"\n");
        let mut h = H::new(&config, |w| {
            w.effect_unavailable = Some("libVideoFX.so not found".into());
            w.denoise_fail = Some(NO_DENOISE.into());
        });
        h.activate(0);
        assert_eq!(h.world().effect_probes, 0, "{on_failure}: no pre-check");
        h.tick(500);
        assert_eq!(h.state(), State::Active, "{on_failure}");
        assert!(h.world().session.is_some(), "{on_failure}");
        assert_eq!(h.snap().error, denoise_error(NO_DENOISE), "{on_failure}");
        assert!(h.snap().denoise, "the request stays");
        // Reported once, not on every poll; the state never moved.
        h.tick(750);
        h.tick(1000);
        let (changes, _) = h.drain();
        let errors = changes
            .iter()
            .filter(|c| matches!(c, Change::Error(_)))
            .count();
        assert_eq!(errors, 1, "{on_failure}: {changes:?}");
        let states: Vec<_> = changes
            .iter()
            .filter_map(|c| match c {
                Change::State(s) => Some(*s),
                _ => None,
            })
            .collect();
        assert_eq!(
            states,
            [State::Starting, State::Active],
            "{on_failure}: the failure moved no state"
        );
        // Ready again (the stage recovered): the error is gone.
        h.world().denoise_status = EffectStatus::Ready;
        h.tick(1250);
        assert_eq!(h.snap().error, "", "{on_failure}");
        assert_eq!(h.ctrl.denoise_failure(), None);
    }
    // With an effect running, the effect is unaffected: still ready, the
    // session still streaming, under `black` too.
    let mut h = H::new("[effect]\nmode = \"blur\"\ndenoise = true\n", |w| {
        w.denoise_fail = Some(NO_DENOISE.into())
    });
    h.activate(0);
    h.tick(500);
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.world().effect_status, EffectStatus::Ready);
    assert_eq!(h.snap().error, denoise_error(NO_DENOISE));
}

#[test]
fn an_effect_method_retries_noise_removal_and_a_new_failure_is_reported_again() {
    let mut h = H::new("", |w| w.denoise_fail = Some(NO_DENOISE.into()));
    h.set_denoise(true, 0).unwrap();
    h.activate(0);
    h.tick(500);
    assert_eq!(h.snap().error, denoise_error(NO_DENOISE));
    // The value already in force: nothing written, but the failed stage
    // is retried. It fails the same way and is reported again.
    let sets = h.world().effect_sets.len();
    // The last good contract, kept at the first frame, saved first.
    h.ctrl.flush_settings();
    fs::remove_file(&h.paths.state).unwrap();
    h.set_denoise(true, 1000).unwrap();
    assert!(!h.paths.state.exists());
    assert_eq!(h.world().effect_sets.len(), sets + 1, "retried");
    assert_eq!(h.snap().error, denoise_error(NO_DENOISE));
    // Another effect method retries it too; it now loads, and the error is
    // gone.
    h.world().denoise_fail = None;
    h.set_strength(0.3, 2000).unwrap();
    assert_eq!(h.world().denoise_status, EffectStatus::Ready);
    assert_eq!(h.snap().error, "");
    assert_eq!(h.world().session_starts, 1);
    // Switching it off ends a failure as well.
    h.world().denoise_fail = Some(NO_DENOISE.into());
    h.set_denoise(true, 2500).unwrap();
    h.world().denoise_status = EffectStatus::Failed(NO_DENOISE.into());
    h.tick(3000);
    assert_eq!(h.snap().error, denoise_error(NO_DENOISE));
    h.set_denoise(false, 3500).unwrap();
    assert_eq!(h.snap().error, "");
    assert_eq!(h.state(), State::Active);
}

#[test]
fn a_reload_or_the_end_of_the_session_clears_a_denoise_failure() {
    let config = "[effect]\ndenoise = true\n\
                  [lifecycle]\nidle_grace_s = 0.0\nstart_debounce_ms = 0\n";
    let mut h = H::new(config, |w| w.denoise_fail = Some(NO_DENOISE.into()));
    h.activate(0);
    h.tick(500);
    assert_eq!(h.snap().error, denoise_error(NO_DENOISE));
    // A reload retries it: now it loads.
    h.world().denoise_fail = None;
    let sets = h.world().effect_sets.len();
    h.reload(config, 1000).unwrap();
    assert_eq!(h.world().effect_sets.len(), sets + 1);
    assert_eq!(h.snap().error, "");
    // It fails again; the consumer leaves: idle, and the error goes with
    // the session.
    h.world().denoise_status = EffectStatus::Failed(NO_DENOISE.into());
    h.tick(1500);
    assert_eq!(h.snap().error, denoise_error(NO_DENOISE));
    h.consumers(0, 2000);
    h.tick(2100);
    assert_eq!(h.state(), State::Idle);
    assert_eq!(h.snap().error, "");
    assert_eq!(h.ctrl.denoise_failure(), None);
    // Off ends it too.
    h.world().denoise_fail = Some(NO_DENOISE.into());
    h.activate(3000);
    h.tick(3500);
    assert_eq!(h.snap().error, denoise_error(NO_DENOISE));
    h.call(|r| Command::SetEnabled(false, r), 4000).unwrap();
    assert_eq!(h.state(), State::Off);
    assert_eq!(h.snap().error, "");
}

#[test]
fn an_effect_failure_is_in_the_warning_and_a_denoise_failure_in_error() {
    let config = "[effect]\nmode = \"blur\"\ndenoise = true\non_failure = \"none\"\n";
    let effect_reason = "the model did not load";
    // Both fail at once: the effect's text in `Warning`, the denoise one
    // in `Error`; neither hides the other.
    let mut h = H::new(config, |w| {
        w.effect_fail = Some(effect_reason.into());
        w.denoise_fail = Some(NO_DENOISE.into());
    });
    h.activate(0);
    h.tick(500);
    assert_eq!(h.state(), State::Active);
    let snap = h.snap();
    assert_eq!(snap.warning, shown_without(effect_reason));
    assert_eq!(snap.error, denoise_error(NO_DENOISE));
    assert!(h.ctrl.denoise_failure().is_some());
    // The effect recovers: the denoise failure still shows.
    h.world().effect_status = EffectStatus::Ready;
    h.tick(1000);
    assert_eq!(h.snap().error, denoise_error(NO_DENOISE));
    assert_eq!(h.snap().warning, "");
    // Noise removal recovers: nothing left.
    h.world().denoise_status = EffectStatus::Ready;
    h.tick(1500);
    assert_eq!(h.snap().error, "");
    // The other order: noise removal fails first (taken as its own once the
    // next poll confirms it, the effect still ready), then the effect.
    h.world().denoise_status = EffectStatus::Failed(NO_DENOISE.into());
    h.tick(1750);
    assert_eq!(h.snap().error, "", "held for one poll");
    h.tick(2000);
    assert_eq!(h.snap().error, denoise_error(NO_DENOISE));
    h.world().effect_status = EffectStatus::Failed(effect_reason.into());
    h.tick(2500);
    assert_eq!(h.snap().error, denoise_error(NO_DENOISE));
    assert_eq!(h.snap().warning, shown_without(effect_reason));
    h.world().effect_status = EffectStatus::Ready;
    h.tick(3000);
    assert_eq!(h.snap().error, denoise_error(NO_DENOISE));
    assert_eq!(h.snap().warning, "");
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.world().session_starts, 1);
}

#[test]
fn a_fault_ends_a_denoise_failure_with_the_session() {
    let config = "[effect]\nmode = \"blur\"\ndenoise = true\n";
    let mut h = H::new(config, |w| w.denoise_fail = Some(NO_DENOISE.into()));
    h.activate(0);
    h.tick(500);
    assert_eq!(h.snap().error, denoise_error(NO_DENOISE));
    // The effect fails with `black`: the session runs on, black, the
    // denoise failure still in `Error`, the effect's in `Warning`.
    h.world().effect_status = EffectStatus::Failed("processing a frame failed".into());
    h.tick(800);
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.snap().error, denoise_error(NO_DENOISE));
    assert_eq!(h.snap().warning, black("processing a frame failed"));
    // The output fails: a fault, the session stops, and the fault's text
    // is the error; the denoise failure ended with the session.
    h.world().failure = Some(Failure::Output("output pipeline failed: boom".into()));
    h.tick(1000);
    assert_eq!(h.state(), State::Error);
    assert_eq!(h.snap().error, "output pipeline failed: boom");
    assert_eq!(h.snap().warning, "");
    assert_eq!(h.ctrl.denoise_failure(), None);
    // The consumer leaves: the fault is cleared (its text stays, as the
    // latest error), the denoise failure does not come back.
    h.consumers(0, 2000);
    h.tick(2100);
    assert_eq!(h.state(), State::Idle);
    assert!(h.snap().error.starts_with("output pipeline"));
    // An unavailable effect with `none` at start, and noise removal
    // failing too: the effect's text in `Warning` until an effect method
    // selects `none`, the denoise one in `Error`.
    let config = "[effect]\nmode = \"blur\"\ndenoise = true\non_failure = \"none\"\n";
    let mut h = H::new(config, |w| {
        w.effect_unavailable = Some("libVideoFX.so not found".into());
        w.denoise_fail = Some(NO_DENOISE.into());
    });
    h.activate(0);
    h.tick(500);
    assert_eq!(h.applied(), Some(EffectMode::None));
    assert_eq!(h.denoise_applied(), Some(true), "noise removal still tried");
    let snap = h.snap();
    assert_eq!(snap.warning, shown_without("libVideoFX.so not found"));
    assert_eq!(snap.error, denoise_error(NO_DENOISE));
    h.set_effect("none", 1000).unwrap();
    assert_eq!(h.snap().error, denoise_error(NO_DENOISE));
    assert_eq!(h.snap().warning, "");
    assert_eq!(h.state(), State::Active);
}

const GREEN_SCREEN: &str = "NvVFX_Load of the green screen failed (-2)";

const BLUR_DENOISE_NONE: &str =
    "[effect]\nmode = \"blur\"\ndenoise = true\non_failure = \"none\"\n";

/// The `Error` texts published since the last drain.
fn errors_published(h: &mut H) -> Vec<String> {
    h.drain()
        .0
        .into_iter()
        .filter_map(|c| match c {
            Change::Error(e) => Some(e),
            _ => None,
        })
        .collect()
}

/// No `Error` published since the last drain blames noise removal.
fn assert_no_denoise_text(h: &mut H) {
    let errors = errors_published(h);
    assert!(
        errors.iter().all(|e| !e.contains("noise removal")),
        "{errors:?}"
    );
}

#[test]
fn a_failure_of_the_whole_chain_is_the_effects_not_noise_removals() {
    // The green screen of blur fails to load: the element shows the
    // failure on the stage too, with the same reason.
    let mut h = H::new(BLUR_DENOISE_NONE, |w| {
        w.effect_fail = Some(GREEN_SCREEN.into());
        w.effect_fail_chain = true;
    });
    h.activate(0);
    h.tick(500);
    h.tick(750);
    let failed = EffectStatus::Failed(GREEN_SCREEN.into());
    assert_eq!(h.world().denoise_status, failed);
    assert_eq!(h.state(), State::Active);
    let effect_warning = shown_without(GREEN_SCREEN);
    let snap = h.snap();
    assert_eq!(snap.warning, effect_warning);
    assert_eq!(snap.error, "");
    assert_eq!(h.ctrl.denoise_failure(), None);
    assert_no_denoise_text(&mut h);
    // The effect recovers, seen a poll before the stage does: nothing
    // stale is left.
    h.world().effect_status = EffectStatus::Ready;
    h.tick(1000);
    assert_eq!(
        (h.snap().error, h.snap().warning),
        (String::new(), String::new())
    );
    h.world().denoise_status = EffectStatus::Ready;
    h.tick(1250);
    assert_eq!(h.snap().error, "");
    assert_no_denoise_text(&mut h);
    // An effect method retries the chain, which fails again, then loads.
    h.set_strength(0.3, 1500).unwrap();
    assert_eq!(h.world().denoise_status, failed);
    h.tick(1750);
    h.tick(2000);
    assert_eq!(h.snap().warning, effect_warning);
    assert_eq!(h.snap().error, "");
    h.world().effect_fail = None;
    h.set_strength(0.4, 2500).unwrap();
    h.tick(2750);
    h.tick(3000);
    assert_eq!(h.world().denoise_status, EffectStatus::Ready);
    assert_eq!(
        (h.snap().error, h.snap().warning),
        (String::new(), String::new())
    );
    assert_eq!(h.ctrl.denoise_failure(), None);
    assert_no_denoise_text(&mut h);
    assert_eq!(h.world().session_starts, 1);
}

#[test]
fn a_failure_of_the_whole_chain_seen_a_poll_apart_is_still_the_effects() {
    let failed = EffectStatus::Failed(GREEN_SCREEN.into());
    let effect_text = shown_without(GREEN_SCREEN);
    for before in [EffectStatus::Ready, EffectStatus::Loading] {
        // The stage's status shows the failure first: the effect's was
        // read before it changed.
        let mut h = H::new(BLUR_DENOISE_NONE, |w| {
            w.effect_slow = before == EffectStatus::Loading;
        });
        h.activate(0);
        h.tick(500);
        assert_eq!(h.world().effect_status, before);
        h.drain();
        h.world().denoise_status = failed.clone();
        h.tick(1000);
        assert_eq!(h.snap().error, "", "{before:?}: not the stage's yet");
        h.world().effect_status = failed.clone();
        h.tick(1250);
        assert_eq!(h.snap().warning, effect_text, "{before:?}");
        assert_eq!(h.snap().error, "", "{before:?}");
        h.tick(1500);
        assert_eq!(h.snap().warning, effect_text, "{before:?}");
        assert_eq!(h.snap().error, "", "{before:?}");
        assert_eq!(h.ctrl.denoise_failure(), None, "{before:?}");
        assert_no_denoise_text(&mut h);
        // Then it recovers: nothing stale.
        h.world().effect_status = EffectStatus::Ready;
        h.world().denoise_status = EffectStatus::Ready;
        h.tick(1750);
        assert_eq!(h.snap().error, "", "{before:?}");
        assert_eq!(h.snap().warning, "", "{before:?}");

        // The effect's status shows it first.
        let mut h = H::new(BLUR_DENOISE_NONE, |w| {
            w.effect_slow = before == EffectStatus::Loading;
        });
        h.activate(0);
        h.tick(500);
        h.drain();
        h.world().effect_status = failed.clone();
        h.tick(1000);
        assert_eq!(h.snap().warning, effect_text, "{before:?}");
        assert_eq!(h.snap().error, "", "{before:?}");
        h.world().denoise_status = failed.clone();
        h.tick(1250);
        h.tick(1500);
        assert_eq!(h.snap().warning, effect_text, "{before:?}");
        assert_eq!(h.snap().error, "", "{before:?}");
        assert_eq!(h.ctrl.denoise_failure(), None, "{before:?}");
        // The effect recovers before the stage does: nothing stale.
        h.world().effect_status = EffectStatus::Ready;
        h.tick(1750);
        assert_eq!(h.snap().error, "", "{before:?}");
        assert_eq!(h.snap().warning, "", "{before:?}");
        assert_no_denoise_text(&mut h);
    }
}

#[test]
fn a_noise_removal_failure_under_a_healthy_effect_is_still_reported() {
    for slow in [false, true] {
        let mut h = H::new(BLUR_DENOISE_NONE, |w| {
            w.denoise_fail = Some(NO_DENOISE.into());
            w.effect_slow = slow;
        });
        // The first poll (at the start) holds it; the next confirms it.
        h.activate(0);
        assert_eq!(h.snap().error, "", "slow {slow}");
        h.tick(550);
        assert_eq!(h.snap().error, denoise_error(NO_DENOISE), "slow {slow}");
        if slow {
            h.world().effect_status = EffectStatus::Ready;
            h.tick(800);
            assert_eq!(h.snap().error, denoise_error(NO_DENOISE));
        }
        h.tick(1050);
        assert_eq!(errors_published(&mut h), [denoise_error(NO_DENOISE)]);
        assert_eq!(h.state(), State::Active);
        // An effect failure with another reason does not hide it: it is in
        // `Warning`, and goes again.
        h.world().effect_status = EffectStatus::Failed(GREEN_SCREEN.into());
        h.tick(1300);
        assert_eq!(h.snap().warning, shown_without(GREEN_SCREEN));
        assert_eq!(h.snap().error, denoise_error(NO_DENOISE), "slow {slow}");
        assert!(h.ctrl.denoise_failure().is_some());
        h.world().effect_status = EffectStatus::Ready;
        h.tick(1550);
        assert_eq!(h.snap().error, denoise_error(NO_DENOISE), "slow {slow}");
        assert_eq!(h.snap().warning, "", "slow {slow}");
    }
}

#[test]
fn a_denoise_value_that_is_not_a_boolean_fails_closed() {
    let mut h = H::new("", |_| {});
    std::fs::create_dir_all(h.paths.state.parent().unwrap()).unwrap();
    std::fs::write(&h.paths.state, "enabled = true\ndenoise = \"yes\"\n").unwrap();
    h.restart();
    assert_eq!(h.state(), State::Off);
    let e = h.snap().error;
    assert!(
        e.contains(&h.paths.state.display().to_string()) && e.contains("SetEnabled"),
        "{e}"
    );
    h.consumers(1, 0);
    h.tick(1000);
    assert_eq!(h.world().session_starts, 0);
    // SetEnabled rewrites the file without the unreadable value.
    h.call(|r| Command::SetEnabled(true, r), 2000).unwrap();
    assert_eq!(h.persisted().denoise, None);
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
        [
            Change::ConfigSerial(2),
            Change::PipeWireOutput("connecting".into())
        ],
        "only the PipeWire output (and the configuration's serial) changes"
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
    // The start, then its retry without the effects: each parks again.
    assert_eq!(
        w.gpu_log,
        ["build", "park", "unpark", "park", "unpark", "park"]
    );
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

// ---- the live preview's leases (interface revision 5) ----

impl H {
    fn acquire(&mut self, name: &str, fps: u32, ms: u64) -> Result<String, IpcError> {
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let now = self.at(ms);
        assert!(self.ctrl.handle(
            Command::AcquirePreview {
                name: name.into(),
                fps,
                reply: tx,
            },
            now
        ));
        rx.try_recv()
            .expect("the controller replies before returning")
    }

    fn release(&mut self, name: &str, ms: u64) {
        let name = name.to_string();
        self.call(|r| Command::ReleasePreview(name, r), ms).unwrap();
    }

    fn gone(&mut self, name: &str, ms: u64) {
        let now = self.at(ms);
        assert!(
            self.ctrl
                .handle(Command::PreviewOwnerGone(name.into()), now)
        );
    }

    /// `(PreviewConsumers, Consumers)`.
    fn leases(&self) -> (u32, u32) {
        let s = self.snap();
        (s.preview_consumers, s.consumers)
    }

    fn preview_dir(&self) -> PathBuf {
        self.paths
            .runtime
            .clone()
            .unwrap()
            .join("omarchy-broadcast")
    }
}

#[test]
fn a_preview_lease_alone_starts_the_session_after_the_debounce_and_its_end_parks_after_the_grace() {
    let mut h = H::new("", |_| {});
    let path = h.acquire(":1.42", 15, 0).unwrap();
    assert_eq!(
        PathBuf::from(&path),
        h.preview_dir().join("preview.jpg"),
        "the frame file's absolute path"
    );
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(h.preview_dir()).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0o700, "a private directory");
    }
    assert_eq!(h.leases(), (1, 1));
    assert_eq!(h.world().previewing(), Some(15), "encoded from the writer");
    assert_eq!(h.state(), State::Idle, "the start debounce first");
    assert_eq!(h.ctrl.next_wakeup(h.at(0)), Duration::from_millis(300));
    h.tick(299);
    assert_eq!(h.state(), State::Idle);
    h.tick(300);
    assert_eq!(h.state(), State::Active, "a lease is a consumer");
    let (changes, _) = h.drain();
    assert!(
        changes.contains(&Change::PreviewConsumers(1)),
        "{changes:?}"
    );
    assert!(changes.contains(&Change::Consumers(1)), "{changes:?}");
    assert!(
        !changes
            .iter()
            .any(|c| matches!(c, Change::LoopbackConsumers(_))),
        "{changes:?}"
    );

    // Released: no consumer; the grace runs, then idle with the GPU parked.
    h.release(":1.42", 1000);
    assert_eq!(h.leases(), (0, 0));
    assert_eq!(h.world().previewing(), None, "the encoder stopped");
    assert_eq!(h.world().preview_sets, [Some(15), None]);
    h.tick(5999);
    assert_eq!(h.state(), State::Active, "the grace period");
    h.tick(6000);
    assert_eq!(h.state(), State::Idle);
    let w = h.world();
    assert!(w.session.is_none());
    assert!(w.parked, "the GPU parked at idle, as for any consumer");
    assert_eq!((w.parks, w.session_starts, w.session_stops), (1, 1, 1));
    assert!(w.writer.is_some());
}

#[test]
fn one_lease_per_name_at_its_latest_rate_and_the_highest_rate_of_two_holders() {
    let mut h = H::new("", |_| {});
    let a = h.acquire(":1.1", 15, 0).unwrap();
    let again = h.acquire(":1.1", 30, 10).unwrap();
    assert_eq!(a, again, "the same path");
    assert_eq!(h.leases(), (1, 1), "one lease");
    assert_eq!(h.world().previewing(), Some(30), "at the new rate");
    h.acquire(":1.1", 15, 20).unwrap();
    assert_eq!(h.world().previewing(), Some(15));
    h.acquire(":1.2", 30, 30).unwrap();
    assert_eq!(h.leases(), (2, 2));
    assert_eq!(h.world().previewing(), Some(30), "the higher rate held");
    h.release(":1.2", 40);
    assert_eq!(h.leases(), (1, 1));
    assert_eq!(h.world().previewing(), Some(15));
    // The backend is asked again only when the rate or the set changed.
    assert_eq!(
        h.world().preview_sets,
        [Some(15), Some(30), Some(15), Some(30), Some(15)]
    );
    h.acquire(":1.1", 15, 50).unwrap();
    assert_eq!(h.world().preview_sets.len(), 5, "nothing changed");
    h.release(":1.1", 60);
    assert_eq!(h.leases(), (0, 0));
    assert_eq!(h.world().preview_sets.last(), Some(&None));
}

#[test]
fn a_bad_rate_is_refused_and_a_release_without_a_lease_does_nothing() {
    let mut h = H::new("", |_| {});
    for fps in [0, 10, 16, 60, u32::MAX] {
        match h.acquire(":1.5", fps, 0) {
            Err(IpcError::InvalidArgument(m)) => {
                assert!(m.contains("15 or 30"), "{fps}: {m}");
            }
            other => panic!("{fps}: expected InvalidArgument, got {other:?}"),
        }
    }
    assert_eq!(h.leases(), (0, 0));
    assert!(h.world().preview_sets.is_empty(), "the backend never asked");
    h.drain();
    h.release(":1.5", 10);
    h.gone(":1.5", 20);
    assert_eq!(h.leases(), (0, 0));
    assert!(h.drain().0.is_empty(), "nothing announced");
    assert!(h.world().preview_sets.is_empty());
    // A lease at 15 or 30 is granted.
    h.acquire(":1.5", 15, 30).unwrap();
    h.release(":1.5", 40);
    h.acquire(":1.5", 30, 50).unwrap();
    assert_eq!(h.leases(), (1, 1));
}

#[test]
fn a_holder_that_left_the_bus_loses_its_lease_and_others_keep_theirs() {
    let mut h = H::new(
        "[lifecycle]\nstart_debounce_ms = 0\nidle_grace_s = 0\n",
        |_| {},
    );
    h.acquire(":1.7", 30, 0).unwrap();
    h.acquire(":1.8", 15, 0).unwrap();
    assert_eq!(h.state(), State::Active);
    h.gone(":1.7", 100);
    assert_eq!(h.leases(), (1, 1));
    assert_eq!(h.world().previewing(), Some(15));
    assert_eq!(h.state(), State::Active);
    // Another name leaving changes nothing.
    h.drain();
    h.gone(":1.99", 150);
    assert!(h.drain().0.is_empty());
    h.gone(":1.8", 200);
    assert_eq!(h.leases(), (0, 0));
    assert_eq!(h.world().previewing(), None);
    assert_eq!(h.state(), State::Idle, "no grace configured");
}

#[test]
fn preview_leases_add_to_the_other_consumers() {
    let mut h = H::new("", |_| {});
    h.activate(0);
    h.acquire(":1.3", 15, 500).unwrap();
    let s = h.snap();
    assert_eq!(
        (s.consumers, s.loopback_consumers, s.preview_consumers),
        (2, 1, 1)
    );
    // The application leaves; the lease keeps the session, past the grace.
    h.consumers(0, 1000);
    assert_eq!(h.leases(), (1, 1));
    h.tick(10_000);
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.world().session_starts, 1);
    h.release(":1.3", 11_000);
    h.tick(16_000);
    assert_eq!(h.state(), State::Idle);
}

#[test]
fn a_lease_is_granted_while_off_and_the_placeholder_is_encoded_without_a_camera() {
    let mut h = H::new("", |_| {});
    h.call(|r| Command::SetEnabled(false, r), 0).unwrap();
    h.acquire(":1.4", 15, 10).unwrap();
    h.tick(1000);
    assert_eq!(h.state(), State::Off);
    assert_eq!(h.leases(), (1, 1));
    assert_eq!(
        h.world().previewing(),
        Some(15),
        "the writer's black frames"
    );
    assert_eq!(h.world().session_starts, 0, "the camera stays closed");
}

#[test]
fn without_a_runtime_directory_a_lease_fails() {
    let dir = tempfile::tempdir().unwrap();
    for runtime in [None, Some(PathBuf::from("relative/run"))] {
        let paths = Paths {
            config: dir.path().join("none.toml"),
            state: dir.path().join("state.toml"),
            cache: None,
            runtime,
            ledger: None,
        };
        let fake = FakeBackend::default();
        let (published, _notes) = Published::new(preliminary_snapshot(&paths));
        let mut ctrl = Controller::new(fake.clone(), paths, published, None, Instant::now());
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        ctrl.handle(
            Command::AcquirePreview {
                name: ":1.1".into(),
                fps: 15,
                reply: tx,
            },
            Instant::now(),
        );
        match rx.try_recv().unwrap() {
            Err(IpcError::Failed(m)) => assert!(m.contains("XDG_RUNTIME_DIR"), "{m}"),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert_eq!(ctrl.snapshot().preview_consumers, 0);
        assert!(fake.world().preview_sets.is_empty());
    }
}

#[test]
fn the_writer_rebuild_waits_for_the_leases_and_the_new_writer_is_previewed() {
    let mut h = H::new(
        "[lifecycle]\nstart_debounce_ms = 0\nidle_grace_s = 0\n",
        |_| {},
    );
    h.call(|r| Command::SetEnabled(false, r), 0).unwrap();
    h.acquire(":1.6", 15, 0).unwrap();
    h.reload("[output]\nmode = \"1280x720@30\"\n", 100).unwrap();
    h.tick(5000);
    assert_eq!(h.world().writer_starts, 1, "a lease is a consumer");
    h.release(":1.6", 6000);
    assert_eq!(h.world().writer_starts, 2);
    // A lease taken after the rebuild is encoded from the new writer.
    h.acquire(":1.6", 15, 7000).unwrap();
    let w = h.world();
    assert_eq!(w.writer.as_ref().unwrap().1.width, 1280);
    assert_eq!(w.previewing(), Some(15));
}

#[test]
fn shutdown_ends_every_lease() {
    let mut h = H::new("", |_| {});
    h.acquire(":1.1", 15, 0).unwrap();
    h.acquire(":1.2", 30, 0).unwrap();
    h.ctrl.shutdown();
    let w = h.world();
    assert_eq!(w.preview, None);
    assert_eq!(w.preview_sets.last(), Some(&None));
    assert!(w.writer.is_none());
}

// ---- camera image controls ----

use obc_core::config::CameraControlState;
use obc_ipc::CameraControl;
use obc_pipeline::controls::DeviceControl;

fn control(name: &str, max: i32, default: i32, value: i32) -> DeviceControl {
    DeviceControl {
        name: name.into(),
        min: 0,
        max,
        step: 1,
        default,
        value,
    }
}

/// A camera as the user found it: far from its defaults (128, and 1 for
/// backlight compensation).
fn far_off() -> Vec<DeviceControl> {
    vec![
        control("brightness", 255, 128, 128),
        control("contrast", 255, 128, 77),
        control("saturation", 255, 128, 179),
        control("sharpness", 255, 128, 255),
        control("backlight_compensation", 1, 1, 0),
    ]
}

/// The fake world with `usb-a` far off its defaults and `usb-b` with a
/// sharpness of its own only (no other control).
fn with_controls(w: &mut World) {
    w.controls.insert("usb-a".into(), far_off());
    w.controls
        .insert("usb-b".into(), vec![control("sharpness", 7, 3, 5)]);
}

impl H {
    fn get_controls(&mut self, camera: &str, ms: u64) -> Result<Vec<CameraControl>, IpcError> {
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let now = self.at(ms);
        assert!(
            self.ctrl
                .handle(Command::GetCameraControls(camera.into(), tx), now)
        );
        rx.try_recv()
            .expect("the controller replies before returning")
    }

    fn set_control(
        &mut self,
        camera: &str,
        name: &str,
        value: i32,
        ms: u64,
    ) -> Result<(), IpcError> {
        self.call(
            |reply| Command::SetCameraControl {
                camera: camera.into(),
                name: name.into(),
                value,
                reply,
            },
            ms,
        )
    }

    fn reset_controls(&mut self, camera: &str, ms: u64) -> Result<(), IpcError> {
        self.call(|r| Command::ResetCameraControls(camera.into(), r), ms)
    }

    /// The writes to the fake devices since the last call.
    fn writes(&mut self) -> Vec<(String, String, i32)> {
        std::mem::take(&mut self.world().control_writes)
    }

    /// The value of control `name` on the fake device of `camera`.
    fn device(&self, camera: &str, name: &str) -> i32 {
        self.world().controls[camera]
            .iter()
            .find(|c| c.name == name)
            .unwrap()
            .value
    }

    /// The `CameraControls` property: (camera, [(name, value, stored)]).
    fn view(&self) -> (String, Vec<(String, i32, bool)>) {
        let v = self.snap().camera_controls.expect("revision 6");
        (
            v.camera,
            v.controls
                .into_iter()
                .map(|c| (c.name, c.value, c.stored))
                .collect(),
        )
    }

    /// The `CameraControls` changes announced since the last drain.
    fn control_changes(&mut self) -> Vec<CameraControls> {
        self.drain()
            .0
            .into_iter()
            .filter_map(|c| match c {
                Change::CameraControls(v) => Some(v),
                _ => None,
            })
            .collect()
    }

    fn stored(&mut self, camera: &str) -> Option<CameraControlState> {
        self.persisted().camera_controls.get(camera).cloned()
    }
}

fn w(camera: &str, name: &str, value: i32) -> (String, String, i32) {
    (camera.into(), name.into(), value)
}

fn entry(name: &str, value: i32, stored: bool) -> (String, i32, bool) {
    (name.into(), value, stored)
}

/// What a `ResetCameraControls` of a closed camera leaves (revision 7:
/// the five image controls' names in `reset_names`, not `reset_all`),
/// with `values` stored after it.
fn image_reset(values: &[(&str, i32)]) -> CameraControlState {
    CameraControlState {
        reset_names: obc_core::config::CAMERA_CONTROL_NAMES
            .iter()
            .map(|n| n.to_string())
            .collect(),
        ..stored(values, false)
    }
}

fn stored(values: &[(&str, i32)], reset_all: bool) -> CameraControlState {
    CameraControlState {
        values: values.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        reset_all,
        reset_names: Default::default(),
    }
}

#[test]
fn get_camera_controls_reads_the_device_in_any_state() {
    let mut h = H::new("", with_controls);
    let idle = h.get_controls("", 0).unwrap();
    assert_eq!(idle.len(), 5);
    assert_eq!(
        idle[3],
        CameraControl {
            name: "sharpness".into(),
            min: 0,
            max: 255,
            step: 1,
            default: 128,
            value: 255,
            stored: false,
        }
    );
    let names: Vec<&str> = idle.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, obc_ipc::CAMERA_CONTROL_NAMES, "allowlist order");
    assert_eq!(h.get_controls("usb-a", 0).unwrap(), idle);
    // Another camera than ActiveCamera, by its id: its own controls.
    let b = h.get_controls("usb-b", 0).unwrap();
    assert_eq!(
        b.iter()
            .map(|c| (c.name.as_str(), c.value))
            .collect::<Vec<_>>(),
        [("sharpness", 5)]
    );
    // The property shows ActiveCamera's, from the start.
    assert_eq!(h.view().0, "usb-a");
    assert_eq!(h.view().1[3], entry("sharpness", 255, false));
    // While active, the same reads; nothing is ever written by a read.
    h.activate(0);
    assert_eq!(h.get_controls("", 1000).unwrap(), idle);
    assert!(h.writes().is_empty());

    // Exact ids only; "" with no camera at all is Unavailable.
    match h.get_controls("Alpha", 1000) {
        Err(IpcError::NoSuchCamera(m)) => assert!(m.contains("\"Alpha\""), "{m}"),
        other => panic!("expected NoSuchCamera, got {other:?}"),
    }
    assert!(matches!(
        h.get_controls("/dev/video0", 1000),
        Err(IpcError::NoSuchCamera(_))
    ));
    // A device that cannot be read is Failed.
    h.world().fail_control_query = vec!["usb-b".into()];
    assert!(matches!(
        h.get_controls("usb-b", 1000),
        Err(IpcError::Failed(_))
    ));
    h.world().cameras.clear();
    h.rescan(2000);
    assert!(matches!(
        h.get_controls("", 2000),
        Err(IpcError::Unavailable(_))
    ));
    assert_eq!(h.view(), (String::new(), Vec::new()));
}

#[test]
fn set_while_active_writes_then_stores_and_announces() {
    let mut h = H::new("", with_controls);
    h.activate(0);
    h.drain();
    h.set_control("", "sharpness", 128, 1000).unwrap();
    assert_eq!(h.writes(), [w("usb-a", "sharpness", 128)]);
    assert_eq!(h.device("usb-a", "sharpness"), 128);
    assert_eq!(
        h.stored("usb-a"),
        Some(stored(&[("sharpness", 128)], false))
    );
    // The property follows, announced; the device's value, stored.
    assert_eq!(h.view().1[3], entry("sharpness", 128, true));
    let changes = h.control_changes();
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0].camera, "usb-a");
    assert_eq!(
        Change::CameraControls(changes[0].clone()).name(),
        "CameraControls"
    );
    // A value equal to the default is stored all the same; by id too.
    h.set_control("usb-a", "saturation", 128, 1100).unwrap();
    assert_eq!(h.writes(), [w("usb-a", "saturation", 128)]);
    assert_eq!(
        h.stored("usb-a"),
        Some(stored(&[("saturation", 128), ("sharpness", 128)], false))
    );
    assert_eq!(h.state(), State::Active, "the session is untouched");
    assert_eq!(h.world().session_starts, 1);
}

#[test]
fn set_while_the_camera_is_not_open_only_stores() {
    let mut h = H::new("", with_controls);
    h.set_control("", "sharpness", 100, 0).unwrap();
    assert!(h.writes().is_empty(), "nothing written while idle");
    assert_eq!(h.device("usb-a", "sharpness"), 255);
    assert_eq!(
        h.stored("usb-a"),
        Some(stored(&[("sharpness", 100)], false))
    );
    // Reported as it will be: the stored value.
    assert_eq!(h.view().1[3], entry("sharpness", 100, true));
    assert_eq!(h.get_controls("", 0).unwrap()[3].value, 100);
    assert_eq!(h.control_changes().len(), 1);
    // Off: stored only.
    h.call(|r| Command::SetEnabled(false, r), 500).unwrap();
    h.set_control("", "brightness", 1, 600).unwrap();
    h.reset_controls("usb-b", 600).unwrap();
    assert!(h.writes().is_empty());
    assert_eq!(
        h.stored("usb-a"),
        Some(stored(&[("brightness", 1), ("sharpness", 100)], false))
    );
    assert_eq!(h.stored("usb-b"), Some(image_reset(&[])));

    // Error (no camera opens): stored only.
    let mut h = H::new("", |w| {
        with_controls(w);
        w.fail_ingest = vec!["usb-a".into(), "usb-b".into()];
    });
    h.consumers(1, 0);
    h.tick(300);
    assert_eq!(h.state(), State::Error);
    h.set_control("", "contrast", 120, 400).unwrap();
    h.reset_controls("usb-b", 400).unwrap();
    assert!(h.writes().is_empty());
    assert_eq!(h.stored("usb-a"), Some(stored(&[("contrast", 120)], false)));
    assert_eq!(h.stored("usb-b"), Some(image_reset(&[])));

    // A camera that is not the session's, while it starts and while it
    // streams: stored only.
    let mut h = H::new("", with_controls);
    h.world().frames = false;
    h.consumers(1, 0);
    h.tick(300);
    assert_eq!(h.state(), State::Starting);
    h.writes();
    h.set_control("usb-b", "sharpness", 6, 350).unwrap();
    h.world().frames = true;
    h.tick(400);
    assert_eq!(h.state(), State::Active);
    h.set_control("usb-b", "sharpness", 4, 450).unwrap();
    h.reset_controls("usb-b", 500).unwrap();
    assert!(h.writes().is_empty());
    assert_eq!(h.device("usb-b", "sharpness"), 5);
    assert_eq!(h.stored("usb-b"), Some(image_reset(&[])));
}

#[test]
fn set_and_reset_while_starting_write_to_the_open_camera() {
    let mut h = H::new("", with_controls);
    h.world().frames = false;
    h.consumers(1, 0);
    h.tick(300);
    assert_eq!(h.state(), State::Starting);
    assert_eq!(h.world().streaming(), Some("usb-a"));
    h.writes();
    h.drain();
    // The ingest is switched to the camera: open, written at once.
    h.set_control("", "contrast", 120, 350).unwrap();
    assert_eq!(h.writes(), [w("usb-a", "contrast", 120)]);
    assert_eq!(h.device("usb-a", "contrast"), 120);
    assert_eq!(h.stored("usb-a"), Some(stored(&[("contrast", 120)], false)));
    assert_eq!(h.view().1[1], entry("contrast", 120, true));
    assert_eq!(h.control_changes().len(), 1);
    h.reset_controls("", 400).unwrap();
    assert_eq!(
        h.writes(),
        [
            w("usb-a", "brightness", 128),
            w("usb-a", "contrast", 128),
            w("usb-a", "saturation", 128),
            w("usb-a", "sharpness", 128),
            w("usb-a", "backlight_compensation", 1),
        ]
    );
    assert_eq!(h.stored("usb-a"), None, "no reset left pending");
    let view = h.view().1;
    assert!(
        view.iter().all(|(n, v, s)| !s
            && *v
                == if n == "backlight_compensation" {
                    1
                } else {
                    128
                }),
        "{view:?}"
    );
    // The camera that took over after a failed one is the open one.
    let mut h = H::new("", |w| {
        with_controls(w);
        w.fail_ingest = vec!["usb-a".into()];
        w.frames = false;
    });
    h.consumers(1, 0);
    h.tick(300);
    assert_eq!(h.state(), State::Starting);
    assert_eq!(h.world().streaming(), Some("usb-b"));
    h.writes();
    h.set_control("usb-a", "sharpness", 1, 350).unwrap();
    assert!(h.writes().is_empty(), "usb-a failed: not open");
    h.set_control("", "sharpness", 6, 350).unwrap();
    assert_eq!(h.writes(), [w("usb-b", "sharpness", 6)]);
}

#[test]
fn a_rename_of_the_session_camera_applies_what_is_stored_under_the_new_id() {
    // The first scan ran before udev added the links: the id is the node.
    let early = cam("/dev/video2", "Beta Cam", Some("2222:0002"), "/dev/video2");
    let mut h = H::new("", |w| {
        w.cameras = vec![early.clone()];
        w.controls
            .insert("/dev/video2".into(), vec![control("sharpness", 7, 3, 5)]);
    });
    // Stored by an earlier run, under the by-id name.
    fs::create_dir_all(h.paths.state.parent().unwrap()).unwrap();
    fs::write(
        &h.paths.state,
        "enabled = true\n[camera_controls.\"usb-b\"]\nvalues = { sharpness = 1 }\n\
         reset_all = true\n",
    )
    .unwrap();
    h.restart();
    // While idle, a rename writes nothing.
    {
        let mut w = h.world();
        w.cameras = vec![beta()];
        let device = w.controls.remove("/dev/video2").unwrap();
        w.controls.insert("usb-b".into(), device);
    }
    h.rescan(100);
    assert_eq!(h.snap().active_camera, "usb-b");
    assert!(h.writes().is_empty());
    assert_eq!(
        h.view(),
        ("usb-b".into(), vec![entry("sharpness", 1, true)])
    );
    // Streaming under the early id: nothing stored under it.
    {
        let mut w = h.world();
        w.cameras = vec![early.clone()];
        let device = w.controls.remove("usb-b").unwrap();
        w.controls.insert("/dev/video2".into(), device);
    }
    h.rescan(200);
    h.activate(300);
    assert_eq!(h.snap().active_camera, "/dev/video2");
    assert!(h.writes().is_empty());
    h.drain();
    // Renamed while streaming: the reset, then the value, as at an open.
    {
        let mut w = h.world();
        w.cameras = vec![beta()];
        let device = w.controls.remove("/dev/video2").unwrap();
        w.controls.insert("usb-b".into(), device);
    }
    h.rescan(3000);
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.snap().active_camera, "usb-b");
    assert_eq!(
        h.writes(),
        [w("usb-b", "sharpness", 3), w("usb-b", "sharpness", 1)]
    );
    assert_eq!(h.device("usb-b", "sharpness"), 1);
    assert_eq!(h.stored("usb-b"), Some(stored(&[("sharpness", 1)], false)));
    assert_eq!(
        h.view(),
        ("usb-b".into(), vec![entry("sharpness", 1, true)])
    );
    let changes = h.control_changes();
    assert_eq!(changes.last().map(|v| v.camera.as_str()), Some("usb-b"));
    assert_eq!(h.world().ingests, ["/dev/video2"], "no camera switch");
}

#[test]
fn bad_names_and_values_are_invalid_and_change_nothing() {
    let mut h = H::new("", |w| {
        with_controls(w);
        // A control with a step of 2 from 1.
        w.controls.get_mut("usb-a").unwrap()[0] = DeviceControl {
            name: "brightness".into(),
            min: 1,
            max: 9,
            step: 2,
            default: 5,
            value: 5,
        };
    });
    h.activate(0);
    h.writes();
    let invalid = |r: Result<(), IpcError>| match r {
        Err(IpcError::InvalidArgument(m)) => m,
        other => panic!("expected InvalidArgument, got {other:?}"),
    };
    let m = invalid(h.set_control("", "zoom_absolute", 1, 1000));
    assert!(
        m.contains("\"zoom_absolute\" is not a camera control"),
        "{m}"
    );
    assert!(m.contains("brightness, contrast, saturation, sharpness, backlight_compensation"));
    let m = invalid(h.set_control("", "exposure_dynamic_framerate", 0, 1000));
    assert!(m.contains("not a camera control"), "{m}");
    // usb-b exposes sharpness only.
    let m = invalid(h.set_control("usb-b", "contrast", 1, 1000));
    assert!(
        m.contains("Beta Cam (usb-b) does not expose contrast"),
        "{m}"
    );
    for (name, value, want) in [
        (
            "sharpness",
            256,
            "sharpness 256 is outside its range 0..=255",
        ),
        ("sharpness", -1, "sharpness -1 is outside its range 0..=255"),
        ("backlight_compensation", 2, "0..=1"),
        (
            "brightness",
            4,
            "brightness 4 is not a whole number of steps of 2 from 1",
        ),
        ("brightness", 11, "outside its range 1..=9"),
    ] {
        let m = invalid(h.set_control("", name, value, 1000));
        assert!(m.contains(want), "{name} {value}: {m}");
        assert!(m.contains("usb-a"), "{m}");
    }
    let m = invalid(h.set_control("usb-b", "sharpness", 8, 1000));
    assert!(m.contains("0..=7"), "{m}");
    assert!(h.writes().is_empty());
    assert!(!h.paths.state.exists() || h.persisted().camera_controls.is_empty());
    // The camera errors.
    assert!(matches!(
        h.set_control("usb-z", "sharpness", 1, 1000),
        Err(IpcError::NoSuchCamera(_))
    ));
    assert!(matches!(
        h.reset_controls("usb-z", 1000),
        Err(IpcError::NoSuchCamera(_))
    ));
    h.world().cameras.clear();
    h.rescan(2000);
    assert!(matches!(
        h.set_control("", "sharpness", 1, 2000),
        Err(IpcError::Unavailable(_))
    ));
    assert!(matches!(
        h.reset_controls("", 2000),
        Err(IpcError::Unavailable(_))
    ));
}

#[test]
fn stored_values_survive_a_restart_and_apply_at_the_next_open() {
    let mut h = H::new("", with_controls);
    h.set_control("", "sharpness", 128, 0).unwrap();
    h.set_control("", "saturation", 128, 0).unwrap();
    h.set_control("", "backlight_compensation", 1, 0).unwrap();
    // Saved after the settings write-behind delay.
    h.ctrl.flush_settings();
    let text = fs::read_to_string(&h.paths.state).unwrap();
    assert!(
        text.contains(
            "[camera_controls.\"usb-a\"]\nvalues = { backlight_compensation = 1, saturation = \
             128, sharpness = 128 }\n"
        ),
        "{text}"
    );
    h.restart();
    assert_eq!(h.view().1[3], entry("sharpness", 128, true));
    assert!(h.writes().is_empty());
    h.activate(1000);
    // At the open, before the first frame, in allowlist order.
    assert_eq!(
        h.writes(),
        [
            w("usb-a", "saturation", 128),
            w("usb-a", "sharpness", 128),
            w("usb-a", "backlight_compensation", 1),
        ]
    );
    // Closing restores nothing: the user's values stay on the device.
    h.consumers(0, 2000);
    h.tick(10_000);
    assert_eq!(h.state(), State::Idle);
    assert!(h.writes().is_empty());
    assert_eq!(h.device("usb-a", "sharpness"), 128);
    // Each open writes them again (the device may have been changed
    // behind the daemon's back).
    h.world().controls.get_mut("usb-a").unwrap()[3].value = 0;
    h.activate(20_000);
    assert_eq!(h.writes().len(), 3);
    assert_eq!(h.device("usb-a", "sharpness"), 128);
}

#[test]
fn reset_while_streaming_writes_the_defaults_and_forgets() {
    let mut h = H::new("", with_controls);
    h.set_control("", "sharpness", 10, 0).unwrap();
    h.activate(100);
    h.writes();
    h.drain();
    h.reset_controls("", 1000).unwrap();
    assert_eq!(
        h.writes(),
        [
            w("usb-a", "brightness", 128),
            w("usb-a", "contrast", 128),
            w("usb-a", "saturation", 128),
            w("usb-a", "sharpness", 128),
            w("usb-a", "backlight_compensation", 1),
        ]
    );
    assert_eq!(h.stored("usb-a"), None);
    assert!(
        !fs::read_to_string(&h.paths.state)
            .unwrap()
            .contains("camera_controls")
    );
    let view = h.view().1;
    assert!(view.iter().all(|(_, _, stored)| !stored), "{view:?}");
    assert_eq!(view[2], entry("saturation", 128, false));
    assert_eq!(h.control_changes().len(), 1);
    // The next open writes nothing: nothing stored, no reset pending.
    h.consumers(0, 2000);
    h.tick(10_000);
    h.activate(20_000);
    assert!(h.writes().is_empty());
}

#[test]
fn reset_while_idle_is_pending_until_the_next_open() {
    let mut h = H::new("", with_controls);
    h.set_control("", "sharpness", 10, 0).unwrap();
    h.reset_controls("", 100).unwrap();
    assert!(h.writes().is_empty(), "nothing written while idle");
    assert_eq!(h.stored("usb-a"), Some(image_reset(&[])));
    let text = fs::read_to_string(&h.paths.state).unwrap();
    assert!(
        text.contains(
            "[camera_controls.\"usb-a\"]\nreset_names = [\"backlight_compensation\", \"brightness\", \"contrast\", \"saturation\", \"sharpness\"]\n"
        ),
        "{text}"
    );
    // Reported as they will be: the defaults, none stored.
    let (_, view) = h.view();
    assert_eq!(view[1], entry("contrast", 128, false));
    assert_eq!(view[4], entry("backlight_compensation", 1, false));
    // A value set after the reset is stored with the reset still pending.
    h.set_control("", "saturation", 100, 200).unwrap();
    assert_eq!(h.stored("usb-a"), Some(image_reset(&[("saturation", 100)])));
    assert_eq!(h.view().1[2], entry("saturation", 100, true));
    // Survives a restart; the open writes the defaults, then the value.
    h.restart();
    h.activate(1000);
    assert_eq!(
        h.writes(),
        [
            w("usb-a", "brightness", 128),
            w("usb-a", "contrast", 128),
            w("usb-a", "saturation", 128),
            w("usb-a", "sharpness", 128),
            w("usb-a", "backlight_compensation", 1),
            w("usb-a", "saturation", 100),
        ]
    );
    assert_eq!(
        h.stored("usb-a"),
        Some(stored(&[("saturation", 100)], false))
    );
    // A reset alone leaves nothing once applied.
    h.consumers(0, 2000);
    h.tick(10_000);
    h.reset_controls("", 11_000).unwrap();
    h.activate(12_000);
    assert_eq!(h.writes().len(), 5);
    assert_eq!(h.stored("usb-a"), None);
}

#[test]
fn a_stored_value_the_device_no_longer_takes_is_skipped_and_kept() {
    let mut h = H::new("", with_controls);
    h.set_control("", "sharpness", 200, 0).unwrap();
    h.set_control("", "contrast", 90, 0).unwrap();
    h.set_control("", "brightness", 7, 0).unwrap();
    {
        let mut w = h.world();
        let cs = w.controls.get_mut("usb-a").unwrap();
        // A narrower sharpness (another firmware), and no brightness.
        cs[3].max = 150;
        cs.remove(0);
    }
    h.activate(1000);
    assert_eq!(h.writes(), [w("usb-a", "contrast", 90)]);
    assert_eq!(h.state(), State::Active);
    // The view shows what the device has: the value skipped is not in
    // force, though still stored.
    assert_eq!(
        h.view().1,
        [
            entry("contrast", 90, true),
            entry("saturation", 179, false),
            entry("sharpness", 255, true),
            entry("backlight_compensation", 0, false),
        ]
    );
    assert_eq!(
        h.stored("usb-a"),
        Some(stored(
            &[("brightness", 7), ("contrast", 90), ("sharpness", 200)],
            false
        ))
    );
}

#[test]
fn a_failed_write_at_the_open_does_not_fail_the_session() {
    let mut h = H::new("", |w| {
        with_controls(w);
        w.fail_control_writes = vec!["sharpness".into()];
    });
    h.set_control("", "sharpness", 128, 0).unwrap();
    h.set_control("", "saturation", 128, 0).unwrap();
    h.activate(1000);
    assert_eq!(h.writes(), [w("usb-a", "saturation", 128)]);
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.snap().error, "");
    // The view shows what the device has: the write that failed left the
    // device's value, still stored.
    assert_eq!(h.view().1[2], entry("saturation", 128, true));
    assert_eq!(h.view().1[3], entry("sharpness", 255, true));
    assert_eq!(
        h.stored("usb-a"),
        Some(stored(&[("saturation", 128), ("sharpness", 128)], false))
    );
    // A query that fails at the open applies nothing, and shows no
    // control; the session streams on.
    h.consumers(0, 2000);
    h.tick(10_000);
    h.world().fail_control_query = vec!["usb-a".into()];
    h.activate(20_000);
    assert!(h.writes().is_empty());
    assert_eq!(h.view(), ("usb-a".to_string(), Vec::new()));
    assert_eq!(h.state(), State::Active);
}

#[test]
fn a_failed_write_while_streaming_is_failed() {
    let mut h = H::new("", with_controls);
    h.activate(0);
    h.writes();
    h.world().fail_control_writes = vec!["sharpness".into(), "contrast".into()];
    match h.set_control("", "sharpness", 128, 1000) {
        Err(IpcError::Failed(m)) => assert!(m.contains("sharpness"), "{m}"),
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(h.writes().is_empty());
    assert!(!h.paths.state.exists() || h.persisted().camera_controls.is_empty());
    // A reset names what failed, and forgets all the same.
    h.world().fail_control_writes = Vec::new();
    h.set_control("", "saturation", 100, 1100).unwrap();
    h.world().fail_control_writes = vec!["sharpness".into(), "contrast".into()];
    h.writes();
    match h.reset_controls("", 1200) {
        Err(IpcError::Failed(m)) => {
            assert!(m.contains("contrast (") && m.contains("sharpness ("), "{m}");
            assert!(!m.contains("saturation"), "{m}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(
        h.writes(),
        [
            w("usb-a", "brightness", 128),
            w("usb-a", "saturation", 128),
            w("usb-a", "backlight_compensation", 1),
        ]
    );
    assert_eq!(h.stored("usb-a"), None);
    assert_eq!(h.state(), State::Active);
}

#[test]
fn each_camera_keeps_its_own_values_and_a_switch_applies_the_new_ones() {
    let mut h = H::new("", with_controls);
    h.set_control("usb-a", "sharpness", 128, 0).unwrap();
    h.set_control("usb-b", "sharpness", 1, 0).unwrap();
    assert_eq!(
        h.stored("usb-a"),
        Some(stored(&[("sharpness", 128)], false))
    );
    assert_eq!(h.stored("usb-b"), Some(stored(&[("sharpness", 1)], false)));
    // The view is ActiveCamera's: usb-b's value is not in it.
    assert_eq!(h.view().1[3], entry("sharpness", 128, true));
    h.activate(1000);
    assert_eq!(h.writes(), [w("usb-a", "sharpness", 128)]);
    h.drain();
    // A switch while streaming opens usb-b with its own value.
    h.call(|r| Command::SelectCamera("usb-b".into(), r), 2000)
        .unwrap();
    assert_eq!(h.writes(), [w("usb-b", "sharpness", 1)]);
    assert_eq!(h.device("usb-a", "sharpness"), 128, "usb-a left as set");
    let changes = h.control_changes();
    assert_eq!(changes.last().map(|v| v.camera.as_str()), Some("usb-b"));
    assert_eq!(
        h.view(),
        ("usb-b".to_string(), vec![entry("sharpness", 1, true)])
    );
    // A reset of the camera that does not stream leaves the one that
    // does alone.
    h.reset_controls("usb-a", 3000).unwrap();
    assert!(h.writes().is_empty());
    assert_eq!(h.stored("usb-a"), Some(image_reset(&[])));
    assert_eq!(h.stored("usb-b"), Some(stored(&[("sharpness", 1)], false)));
    // Selecting while idle changes ActiveCamera: the view follows,
    // announced, nothing written.
    h.consumers(0, 4000);
    h.tick(20_000);
    h.drain();
    h.call(|r| Command::SelectCamera("usb-a".into(), r), 21_000)
        .unwrap();
    assert!(h.writes().is_empty());
    let changes = h.control_changes();
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0].camera, "usb-a");
    assert_eq!(
        h.view().1[3],
        entry("sharpness", 128, false),
        "reset pending"
    );
}

#[test]
fn a_fail_over_and_a_mode_change_reopen_apply_the_controls() {
    let mut h = H::new("", |w| {
        with_controls(w);
        w.fail_ingest = vec!["usb-a".into()];
        w.follow_request = true;
    });
    h.set_control("usb-a", "sharpness", 128, 0).unwrap();
    h.set_control("usb-b", "sharpness", 2, 0).unwrap();
    // usb-a fails to open: usb-b takes over, with its value.
    h.activate(1000);
    assert_eq!(h.world().streaming(), Some("usb-b"));
    assert_eq!(h.writes(), [w("usb-b", "sharpness", 2)]);
    // A reload that changes the camera mode reopens it: applied again.
    h.reload("[camera]\nmode = \"1280x720@30\"\n", 2000)
        .unwrap();
    assert_eq!(h.world().ingests.last().map(String::as_str), Some("usb-b"));
    assert_eq!(h.writes(), [w("usb-b", "sharpness", 2)]);
}

#[test]
fn the_view_is_refreshed_on_a_rescan_only_when_asked() {
    let mut h = H::new("", with_controls);
    let queries = h.world().control_queries;
    // Ticks and consumers do not query the device.
    for ms in [100, 1000, 30_000] {
        h.tick(ms);
    }
    assert_eq!(h.world().control_queries, queries);
    // A rescan reads it again (the device changed behind the daemon's
    // back), and announces the difference.
    h.world().controls.get_mut("usb-a").unwrap()[3].value = 3;
    h.drain();
    h.rescan(31_000);
    assert_eq!(h.view().1[3], entry("sharpness", 3, false));
    assert_eq!(h.control_changes().len(), 1);
    // A camera that cannot be read shows none, and is logged once.
    h.world().fail_control_query = vec!["usb-a".into()];
    h.rescan(32_000);
    h.rescan(33_000);
    assert_eq!(h.view(), ("usb-a".to_string(), Vec::new()));
    assert_eq!(h.ctrl.controls_unreadable.as_deref(), Some("usb-a"));
    h.world().fail_control_query = Vec::new();
    h.rescan(34_000);
    assert_eq!(h.view().1.len(), 5);
    assert_eq!(h.ctrl.controls_unreadable, None);
}

#[test]
fn bad_camera_control_entries_in_the_state_file_do_not_turn_the_daemon_off() {
    let mut h = H::new("", with_controls);
    fs::create_dir_all(h.paths.state.parent().unwrap()).unwrap();
    fs::write(
        &h.paths.state,
        "enabled = true\n[camera_controls.\"usb-a\"]\nvalues = { sharpness = 99, focus = 1, \
         contrast = 1.5 }\n",
    )
    .unwrap();
    h.restart();
    assert_eq!(h.snap().error, "");
    assert!(h.snap().enabled);
    h.activate(1000);
    assert_eq!(h.writes(), [w("usb-a", "sharpness", 99)]);
}

#[test]
fn the_allowlist_is_the_same_on_every_side() {
    let names: Vec<&str> = IMAGE_CONTROLS.iter().map(|(n, _)| *n).collect();
    assert_eq!(names, obc_ipc::CAMERA_CONTROL_NAMES);
    assert_eq!(names, obc_core::config::CAMERA_CONTROL_NAMES);
}

#[test]
fn the_view_is_read_again_once_at_the_first_frame() {
    let mut h = H::new("", with_controls);
    h.set_control("", "sharpness", 128, 0).unwrap();
    h.world().frames = false;
    h.consumers(1, 100);
    h.tick(400);
    assert_eq!(h.state(), State::Starting);
    assert_eq!(h.writes(), [w("usb-a", "sharpness", 128)]);
    assert_eq!(h.view().1[3], entry("sharpness", 128, true));
    // The camera changes a control as it starts streaming.
    h.world().controls.get_mut("usb-a").unwrap()[1].value = 64;
    let queries = h.world().control_queries;
    h.tick(500);
    assert_eq!(h.world().control_queries, queries, "no poll while starting");
    h.drain();
    h.world().frames = true;
    h.tick(600);
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.world().control_queries, queries + 1, "one refresh");
    assert_eq!(h.view().1[1], entry("contrast", 64, false));
    assert_eq!(h.view().1[3], entry("sharpness", 128, true));
    assert_eq!(h.control_changes().len(), 1);
    // Streaming on: no more reads.
    for ms in [700, 2000, 30_000] {
        h.tick(ms);
    }
    assert_eq!(h.world().control_queries, queries + 1, "never polled");
    // A switch is an open too: the open's query, then one read at that
    // camera's first frame (here at once), then none.
    let queries = h.world().control_queries;
    h.call(|r| Command::SelectCamera("usb-b".into(), r), 31_000)
        .unwrap();
    assert_eq!(h.world().control_queries, queries + 2);
    h.tick(31_100);
    h.tick(31_200);
    assert_eq!(h.world().control_queries, queries + 2);
}

#[test]
fn a_set_opens_the_node_once_and_reads_back_only_the_control_written() {
    let mut h = H::new("", |w| {
        with_controls(w);
        // A driver that rounds what it is given.
        w.control_write_quirks.insert("sharpness".into(), 130);
    });
    h.activate(0);
    h.writes();
    let counts = |h: &H| {
        let w = h.world();
        (w.control_opens, w.control_queries, w.control_reads)
    };
    let (opens, queries, reads) = counts(&h);
    h.set_control("", "sharpness", 129, 1000).unwrap();
    assert_eq!(h.writes(), [w("usb-a", "sharpness", 129)]);
    // Validated against the last read: no query, only the read-back.
    assert_eq!(counts(&h), (opens + 1, queries, reads + 1));
    // The view shows the value read back; the user's value is stored.
    assert_eq!(h.view().1[3], entry("sharpness", 130, true));
    assert_eq!(
        h.stored("usb-a"),
        Some(stored(&[("sharpness", 129)], false))
    );
    // A reset while open: one open, every default written and read back.
    let (opens, queries, reads) = counts(&h);
    h.reset_controls("", 1100).unwrap();
    assert_eq!(h.writes().len(), 5);
    assert_eq!(counts(&h), (opens + 1, queries + 1, reads + 5));
    // Not open: one open and one query, nothing written or read back.
    h.consumers(0, 2000);
    h.tick(10_000);
    assert_eq!(h.state(), State::Idle);
    let (opens, queries, reads) = counts(&h);
    h.set_control("", "contrast", 3, 11_000).unwrap();
    h.reset_controls("usb-b", 11_000).unwrap();
    assert!(h.writes().is_empty());
    assert_eq!(
        counts(&h),
        (opens + 1, queries + 1, reads),
        "usb-b not shown: no read"
    );
    // The open: one open for the reset, the values and the view.
    h.set_control("", "sharpness", 100, 11_100).unwrap();
    h.reset_controls("", 11_200).unwrap();
    h.set_control("", "saturation", 7, 11_300).unwrap();
    let (opens, queries, _) = counts(&h);
    h.world().frames = false;
    h.consumers(1, 12_000);
    h.tick(12_300);
    assert_eq!(h.state(), State::Starting);
    assert_eq!(h.writes().len(), 6, "the defaults, then the value");
    assert_eq!(counts(&h).0 - opens, 1);
    assert_eq!(counts(&h).1 - queries, 1);
    assert_eq!(h.view().1[2], entry("saturation", 7, true));
}

#[test]
fn a_control_that_cannot_be_read_is_left_out_and_warned_about_once() {
    let mut h = H::new("", |w| {
        with_controls(w);
        w.fail_control_reads = vec!["contrast".into()];
    });
    let got = h.get_controls("", 0).unwrap();
    let names: Vec<&str> = got.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "brightness",
            "saturation",
            "sharpness",
            "backlight_compensation"
        ]
    );
    assert_eq!(h.view().1.len(), 4);
    assert!(
        h.ctrl
            .controls_skipped
            .contains(&("usb-a".to_string(), "contrast".to_string()))
    );
    assert_eq!(h.ctrl.controls_skipped.len(), 1);
    h.get_controls("", 0).unwrap();
    h.rescan(100);
    assert_eq!(h.ctrl.controls_skipped.len(), 1, "warned once");
    // It cannot be set; the others can.
    assert!(matches!(
        h.set_control("", "contrast", 1, 200),
        Err(IpcError::InvalidArgument(_))
    ));
    h.activate(300);
    h.set_control("", "sharpness", 1, 1000).unwrap();
    assert_eq!(h.writes(), [w("usb-a", "sharpness", 1)]);
}

// ---- camera settings (revision 7) ----

use obc_ipc::Setting;
use obc_pipeline::control_node::NodeControl;
use obc_pipeline::controls::{CTRL_FLAG_INACTIVE, CTRL_TYPE_INTEGER};

use crate::backend::fake::FakeDevice;

use crate::backend::fake::cid::{AUTO_EXPOSURE, EXPOSURE, GAIN, PAN_RESET, SHARPNESS};
use crate::backend::fake::{brio_device, node_control};

const EXPOSURE_ID: &str = "camera.exposure.exposure_time_absolute";
const MODE_ID: &str = "camera.exposure.auto_exposure";
const SHARP_ID: &str = "camera.image.sharpness";
const FOV_ID: &str = "camera.focus.field_of_view";
const KEEP_ID: &str = "camera.exposure.keep_framerate";

/// The fake world with `usb-a` a fake MX Brio and `usb-b` with simple
/// image controls.
fn with_brio(w: &mut World) {
    w.devices.insert("usb-a".into(), brio_device());
    w.controls
        .insert("usb-b".into(), vec![control("sharpness", 7, 3, 5)]);
}

type SetReply = tokio::sync::oneshot::Receiver<Result<(), IpcError>>;

impl H {
    fn get_settings(&mut self, camera: &str, ms: u64) -> Result<SettingsTree, IpcError> {
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let now = self.at(ms);
        assert!(
            self.ctrl
                .handle(Command::GetSettings(camera.into(), tx), now)
        );
        rx.try_recv()
            .expect("the controller replies before returning")
    }

    /// A setting of `ActiveCamera`'s published tree.
    fn setting(&mut self, id: &str) -> Setting {
        let tree = self.get_settings("", self.ms_now()).unwrap();
        tree.find(id)
            .unwrap_or_else(|| panic!("no {id} in {tree:?}"))
            .clone()
    }

    /// A time that changes nothing (`GetSettings` does not look at it).
    fn ms_now(&self) -> u64 {
        0
    }

    /// Sends `SetSetting` and returns its reply channel (empty while held).
    fn send_set(
        &mut self,
        camera: &str,
        id: &str,
        value: Option<SettingValue>,
        ms: u64,
    ) -> SetReply {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let now = self.at(ms);
        assert!(self.ctrl.handle(
            Command::SetSetting {
                camera: camera.into(),
                id: id.into(),
                value,
                reply: tx,
            },
            now,
        ));
        rx
    }

    fn set_setting(&mut self, id: &str, value: SettingValue, ms: u64) -> Result<(), IpcError> {
        self.send_set("", id, Some(value), ms)
            .try_recv()
            .expect("answered at once")
    }

    fn reset_settings(&mut self, scope: &str, ms: u64) -> Result<(), IpcError> {
        self.call(
            |reply| Command::ResetSettings {
                camera: String::new(),
                scope: scope.into(),
                reply,
            },
            ms,
        )
    }

    /// The fake MX Brio, as it is now.
    fn brio(&self) -> FakeDevice {
        self.world().devices["usb-a"].clone()
    }

    /// Changes the fake MX Brio.
    fn brio_mut(&self, f: impl FnOnce(&mut FakeDevice)) {
        f(self.world().devices.get_mut("usb-a").unwrap());
    }

    /// The `SettingsChanged` notes since the last call.
    fn signals(&mut self) -> Vec<(u32, u32, Vec<SettingChange>)> {
        self.drain();
        std::mem::take(&mut self.settings_notes)
    }
}

/// The control event a driver sends for control `id` of `d` as it is now,
/// for the changes `changes`.
fn ctrl_event(d: &FakeDevice, id: u32, changes: u32) -> ControlEvent {
    let c = d.control(id);
    ControlEvent::Changed(CtrlEvent {
        id,
        changes,
        type_: c.info.type_,
        value: c.value.value().unwrap_or(0),
        flags: c.info.flags,
        minimum: c.info.minimum,
        maximum: c.info.maximum,
        step: c.info.step,
        default_value: c.info.default_value,
    })
}

fn s(v: &str) -> SettingValue {
    SettingValue::Str(v.into())
}

fn change(id: &str, value: Option<SettingValue>, stored: bool, reason: &str) -> SettingChange {
    SettingChange {
        id: id.into(),
        value,
        stored,
        enabled: reason.is_empty(),
        disabled_reason: reason.into(),
    }
}

const MANUAL_REASON: &str = "Set exposure to manual to change this";

#[test]
fn get_settings_gives_the_tree_from_revision_one_and_no_camera_is_no_error() {
    let mut h = H::new("", with_brio);
    let snap = h.snap();
    assert_eq!(
        (snap.settings_revision, snap.settings_schema_serial),
        (Some(1), Some(1))
    );
    let tree = h.get_settings("", 0).unwrap();
    assert_eq!((tree.revision, tree.serial), (1, 1));
    assert_eq!(tree.camera, "usb-a");
    let groups: Vec<&str> = tree.groups.iter().map(|g| g.label.as_str()).collect();
    assert_eq!(
        groups,
        [
            "Image",
            "White balance",
            "Exposure",
            "Focus & zoom",
            "Background effect",
            "Video",
            "Lighting"
        ]
    );
    let exp = tree.find(EXPOSURE_ID).unwrap();
    assert!(!exp.enabled);
    assert_eq!(exp.disabled_reason, MANUAL_REASON);
    assert_eq!(tree.find(FOV_ID).unwrap().kind.value(), Some(s("65")));
    // By id: the same; another camera's read now, at the current revision.
    assert_eq!(h.get_settings("usb-a", 0).unwrap(), tree);
    let b = h.get_settings("usb-b", 0).unwrap();
    assert_eq!((b.camera.as_str(), b.revision, b.serial), ("usb-b", 1, 1));
    assert_eq!(
        b.find(SHARP_ID).unwrap().kind.value(),
        Some(SettingValue::Int(5))
    );
    assert!(matches!(
        h.get_settings("usb-z", 0),
        Err(IpcError::NoSuchCamera(_))
    ));
    // The published tree is served without reading the device.
    let queries = h.world().control_queries;
    h.get_settings("", 0).unwrap();
    assert_eq!(h.world().control_queries, queries);
    // No camera: only the global groups, a structural change.
    h.world().cameras.clear();
    h.rescan(100);
    let none = h.get_settings("", 100).unwrap();
    assert_eq!((none.revision, none.serial), (2, 2));
    let ids: Vec<&str> = none.groups.iter().map(|g| g.id.as_str()).collect();
    assert_eq!(
        (none.camera.as_str(), ids),
        ("", vec!["background", "video", "lighting"])
    );
    assert_eq!(h.signals(), [(2, 2, Vec::new())]);
}

#[test]
fn a_mode_change_and_the_exposure_it_enables_come_in_one_signal() {
    let mut h = H::new("", with_brio);
    h.activate(0);
    h.signals();
    let rev = h.snap().settings_revision.unwrap();
    let serial = h.snap().settings_schema_serial.unwrap();
    h.set_setting(MODE_ID, s("manual"), 1000).unwrap();
    // Written, the dependent's flag flipped by the device, read again.
    assert_eq!(h.writes(), [w("usb-a", "auto_exposure", 1)]);
    assert_eq!(
        h.brio().control(EXPOSURE).info.flags & CTRL_FLAG_INACTIVE,
        0
    );
    assert_eq!(
        h.signals(),
        [(
            rev + 1,
            serial,
            vec![
                change(MODE_ID, Some(s("manual")), true, ""),
                change(EXPOSURE_ID, Some(SettingValue::Int(1250)), false, ""),
            ]
        )]
    );
    assert_eq!(h.snap().settings_revision, Some(rev + 1));
    // Now the exposure time can be set; back to automatic greys it again.
    h.set_setting(EXPOSURE_ID, SettingValue::Int(500), 1100)
        .unwrap();
    assert_eq!(h.brio().value(EXPOSURE), 500);
    h.set_setting(MODE_ID, s("aperture_priority"), 1200)
        .unwrap();
    let sigs = h.signals();
    assert_eq!(sigs.len(), 2, "{sigs:?}");
    assert_eq!(
        sigs[1].2,
        [
            change(MODE_ID, Some(s("aperture_priority")), true, ""),
            change(
                EXPOSURE_ID,
                Some(SettingValue::Int(500)),
                true,
                MANUAL_REASON
            ),
        ]
    );
    // Stored by name, raw.
    let st = h.stored("usb-a").unwrap();
    assert_eq!(
        st.values.into_iter().collect::<Vec<_>>(),
        [
            ("auto_exposure".to_string(), 3),
            ("exposure_time_absolute".to_string(), 500)
        ]
    );
    // Greyed: Disabled with its reason, nothing written or stored.
    h.writes();
    match h.set_setting(EXPOSURE_ID, SettingValue::Int(600), 1300) {
        Err(IpcError::Disabled(m)) => assert_eq!(m, MANUAL_REASON),
        other => panic!("expected Disabled, got {other:?}"),
    }
    assert!(h.writes().is_empty());
    assert_eq!(
        h.stored("usb-a").unwrap().values["exposure_time_absolute"],
        500
    );
}

#[test]
fn a_set_while_closed_is_stored_and_the_open_writes_masters_first() {
    let mut h = H::new("", with_brio);
    h.set_setting(MODE_ID, s("manual"), 0).unwrap();
    // Closed: the stored mode enables the exposure time at once.
    assert!(h.setting(EXPOSURE_ID).enabled);
    h.set_setting(EXPOSURE_ID, SettingValue::Int(400), 0)
        .unwrap();
    h.set_setting(SHARP_ID, SettingValue::Int(100), 0).unwrap();
    assert!(h.writes().is_empty(), "nothing written while closed");
    assert_eq!(h.brio().value(AUTO_EXPOSURE), 3);
    let exp = h.setting(EXPOSURE_ID);
    assert_eq!(exp.kind.value(), Some(SettingValue::Int(400)));
    assert!(exp.stored);
    // The open: masters (sharpness, the mode) first, then the exposure
    // time, which the fake refuses while inactive.
    h.activate(1000);
    assert_eq!(
        h.writes(),
        [
            w("usb-a", "sharpness", 100),
            w("usb-a", "auto_exposure", 1),
            w("usb-a", "exposure_time_absolute", 400),
        ]
    );
    assert_eq!(h.brio().value(EXPOSURE), 400);
    assert!(h.setting(EXPOSURE_ID).enabled);
    // A stored dependent whose master leaves it inactive is not written.
    h.consumers(0, 2000);
    h.tick(10_000);
    h.set_setting(MODE_ID, s("aperture_priority"), 10_000)
        .unwrap();
    h.activate(11_000);
    assert_eq!(
        h.writes(),
        [w("usb-a", "sharpness", 100), w("usb-a", "auto_exposure", 3)]
    );
    assert_eq!(h.state(), State::Active);
}

#[test]
fn set_setting_refuses_in_the_contract_order() {
    let mut h = H::new("", with_brio);
    h.activate(0);
    h.writes();
    let err = |h: &mut H, id: &str, v: Option<SettingValue>| {
        h.send_set("", id, v, 1000).try_recv().expect("answered")
    };
    assert!(matches!(
        err(&mut h, "camera.image.nope", Some(SettingValue::Int(1))),
        Err(IpcError::UnknownSetting(_))
    ));
    // Disabled before the value is looked at.
    assert!(matches!(
        err(&mut h, EXPOSURE_ID, None),
        Err(IpcError::Disabled(_))
    ));
    for (id, v) in [
        (SHARP_ID, None),
        (SHARP_ID, Some(SettingValue::Int(256))),
        (SHARP_ID, Some(SettingValue::Bool(true))),
        (MODE_ID, Some(s("auto"))),
        (MODE_ID, Some(SettingValue::Int(1))),
        (KEEP_ID, Some(SettingValue::Int(1))),
        ("camera.focus.pan_reset", Some(SettingValue::Bool(false))),
        (FOV_ID, Some(s("120"))),
    ] {
        assert!(
            matches!(
                err(&mut h, id, v.clone()),
                Err(IpcError::InvalidArgument(_))
            ),
            "{id} {v:?}"
        );
    }
    assert!(h.writes().is_empty());
    assert!(!h.paths.state.exists() || h.persisted().camera_controls.is_empty());
    assert!(matches!(
        h.send_set("usb-z", SHARP_ID, Some(SettingValue::Int(1)), 1000)
            .try_recv()
            .unwrap(),
        Err(IpcError::NoSuchCamera(_))
    ));
    // A device write failure is Failed and stores nothing.
    h.brio_mut(|d| d.refuse.push(GAIN));
    assert!(matches!(
        h.set_setting("camera.exposure.gain", SettingValue::Int(10), 1000),
        Err(IpcError::Failed(_))
    ));
    assert!(!h.paths.state.exists() || h.persisted().camera_controls.is_empty());
    // An action fires while open, stores nothing; closed it is refused.
    h.set_setting("camera.focus.pan_reset", SettingValue::Bool(true), 1100)
        .unwrap();
    assert_eq!(h.writes(), [w("usb-a", "pan_reset", 1)]);
    assert!(!h.paths.state.exists() || h.persisted().camera_controls.is_empty());
    h.consumers(0, 2000);
    h.tick(10_000);
    assert!(matches!(
        h.set_setting("camera.focus.pan_reset", SettingValue::Bool(true), 10_000),
        Err(IpcError::Unavailable(_))
    ));
    assert!(h.writes().is_empty());
    // No camera at all: Unavailable, as for the revision-6 methods.
    h.world().cameras.clear();
    h.rescan(11_000);
    assert!(matches!(
        h.set_setting(SHARP_ID, SettingValue::Int(1), 11_000),
        Err(IpcError::Unavailable(_))
    ));
}

#[test]
fn a_set_within_the_window_is_held_replaced_and_answered_when_written() {
    let mut h = H::new("", with_brio);
    h.activate(0);
    h.writes();
    h.set_setting(SHARP_ID, SettingValue::Int(10), 1000)
        .unwrap();
    // Within 50 ms: validated, then held with its reply.
    let mut first = h.send_set("", SHARP_ID, Some(SettingValue::Int(20)), 1010);
    assert!(first.try_recv().is_err(), "held");
    // An invalid one is refused at once, and does not replace it.
    assert!(matches!(
        h.send_set("", SHARP_ID, Some(SettingValue::Int(999)), 1015)
            .try_recv()
            .unwrap(),
        Err(IpcError::InvalidArgument(_))
    ));
    assert!(first.try_recv().is_err(), "still held");
    let mut second = h.send_set("", SHARP_ID, Some(SettingValue::Int(30)), 1020);
    assert!(
        matches!(first.try_recv().unwrap(), Ok(())),
        "superseded: Ok"
    );
    assert!(second.try_recv().is_err());
    // Another setting is not held by sharpness's window.
    h.set_setting("camera.exposure.gain", SettingValue::Int(5), 1030)
        .unwrap();
    assert_eq!(
        h.writes(),
        [w("usb-a", "sharpness", 10), w("usb-a", "gain", 5)]
    );
    // The controller wakes when the window opens.
    assert_eq!(h.ctrl.next_wakeup(h.at(1030)), Duration::from_millis(20));
    h.tick(1049);
    assert!(second.try_recv().is_err());
    h.tick(1050);
    assert!(matches!(second.try_recv().unwrap(), Ok(())));
    assert_eq!(h.writes(), [w("usb-a", "sharpness", 30)]);
    assert_eq!(h.brio().value(SHARPNESS), 30);
    assert_eq!(h.stored("usb-a").unwrap().values["sharpness"], 30);
    // A held write that fails is answered Failed, nothing stored.
    let mut held = h.send_set("", SHARP_ID, Some(SettingValue::Int(40)), 1060);
    h.brio_mut(|d| d.refuse.push(SHARPNESS));
    h.tick(1100);
    assert!(matches!(held.try_recv().unwrap(), Err(IpcError::Failed(_))));
    assert_eq!(h.stored("usb-a").unwrap().values["sharpness"], 30);
    // Any command runs what is due first (a slider never starves it).
    h.brio_mut(|d| d.refuse.clear());
    h.set_setting(SHARP_ID, SettingValue::Int(45), 1105)
        .unwrap();
    let mut held = h.send_set("", SHARP_ID, Some(SettingValue::Int(50)), 1110);
    assert!(held.try_recv().is_err(), "held");
    h.writes();
    h.consumers(1, 1200);
    assert!(matches!(held.try_recv().unwrap(), Ok(())));
    assert_eq!(h.writes(), [w("usb-a", "sharpness", 50)]);
    // Closed: nothing written, nothing held.
    h.consumers(0, 2000);
    h.tick(10_000);
    h.set_setting(SHARP_ID, SettingValue::Int(60), 10_000)
        .unwrap();
    h.set_setting(SHARP_ID, SettingValue::Int(61), 10_001)
        .unwrap();
    assert!(h.writes().is_empty());
}

#[test]
fn over_a_hundred_writes_a_second_are_rate_limited() {
    let extra: Vec<NodeControl> = (0..30)
        .map(|i| {
            node_control(
                0x0098_0a00 + i,
                &format!("Extra {i}"),
                CTRL_TYPE_INTEGER,
                (0, 1000, 1),
                0,
                0,
                0,
            )
        })
        .collect();
    let mut h = H::new("", |w| {
        w.devices.insert(
            "usb-a".into(),
            FakeDevice {
                controls: extra,
                ..FakeDevice::default()
            },
        );
    });
    h.activate(0);
    for k in 0..100u64 {
        let id = format!("camera.other.extra_{}", k % 30);
        h.set_setting(&id, SettingValue::Int(k as i64 + 1), 1000 + k * 5)
            .unwrap();
    }
    match h.set_setting("camera.other.extra_10", SettingValue::Int(999), 1500) {
        Err(IpcError::RateLimited(m)) => assert!(m.contains("100"), "{m}"),
        other => panic!("expected RateLimited, got {other:?}"),
    }
    assert_ne!(h.stored("usb-a").unwrap().values["extra_10"], 999);
    // A second after the first write, one is free again.
    h.set_setting("camera.other.extra_10", SettingValue::Int(999), 2000)
        .unwrap();
    assert_eq!(h.stored("usb-a").unwrap().values["extra_10"], 999);
}

#[test]
fn reset_settings_by_scope_open_and_closed() {
    let mut h = H::new("", with_brio);
    for (id, v) in [
        (SHARP_ID, SettingValue::Int(10)),
        ("camera.exposure.gain", SettingValue::Int(7)),
        (MODE_ID, s("manual")),
        (EXPOSURE_ID, SettingValue::Int(300)),
    ] {
        h.set_setting(id, v, 0).unwrap();
    }
    // Closed, one setting: forgotten, its name left for the next open.
    h.reset_settings(&format!("setting:{SHARP_ID}"), 100)
        .unwrap();
    let st = h.stored("usb-a").unwrap();
    assert!(!st.values.contains_key("sharpness"));
    assert_eq!(st.reset_names.iter().collect::<Vec<_>>(), ["sharpness"]);
    assert!(!st.reset_all);
    assert_eq!(
        h.setting(SHARP_ID).kind.value(),
        Some(SettingValue::Int(128))
    );
    // A group.
    h.reset_settings("group:exposure", 100).unwrap();
    let st = h.stored("usb-a").unwrap();
    assert!(st.values.is_empty(), "{st:?}");
    assert!(st.reset_names.contains("gain") && st.reset_names.contains("auto_exposure"));
    assert!(h.writes().is_empty());
    // The whole camera: reset_all, the names subsumed.
    h.set_setting(SHARP_ID, SettingValue::Int(10), 200).unwrap();
    h.reset_settings("camera", 200).unwrap();
    let st = h.stored("usb-a").unwrap();
    assert!(st.reset_all && st.values.is_empty() && st.reset_names.is_empty());
    // Bad scopes.
    for scope in ["", "everything", "group:", "setting:"] {
        assert!(
            matches!(
                h.reset_settings(scope, 300),
                Err(IpcError::InvalidArgument(_))
            ),
            "{scope:?}"
        );
    }
    assert!(matches!(
        h.reset_settings("group:nope", 300),
        Err(IpcError::UnknownSetting(_))
    ));
    assert!(matches!(
        h.reset_settings("setting:camera.image.nope", 300),
        Err(IpcError::UnknownSetting(_))
    ));
    // The open writes every default, dependents first (the exposure time
    // is inactive: not written), then nothing stored.
    h.activate(1000);
    let writes = h.writes();
    let names: Vec<&str> = writes.iter().map(|(_, n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "brightness",
            "contrast",
            "sharpness",
            "white_balance_automatic",
            "auto_exposure",
            "gain",
            "exposure_dynamic_framerate",
        ],
        "{writes:?}"
    );
    assert_eq!(h.world().xu_writes, [("usb-a".into(), "90".into())]);
    assert_eq!(h.stored("usb-a"), None);
    // Open: a scope writes its defaults now, dependents first.
    h.set_setting(MODE_ID, s("manual"), 2000).unwrap();
    h.set_setting(EXPOSURE_ID, SettingValue::Int(300), 2100)
        .unwrap();
    h.writes();
    h.reset_settings("group:exposure", 2200).unwrap();
    let writes = h.writes();
    assert_eq!(
        writes,
        [
            w("usb-a", "exposure_time_absolute", 156),
            w("usb-a", "auto_exposure", 3),
            w("usb-a", "gain", 0),
            w("usb-a", "exposure_dynamic_framerate", 0),
        ]
    );
    assert_eq!(h.stored("usb-a"), None);
    assert!(!h.setting(EXPOSURE_ID).enabled);
    // A refused default fails the call, naming it; forgotten all the same.
    h.set_setting("camera.exposure.gain", SettingValue::Int(9), 2300)
        .unwrap();
    h.brio_mut(|d| d.refuse.push(GAIN));
    match h.reset_settings("setting:camera.exposure.gain", 2400) {
        Err(IpcError::Failed(m)) => assert!(m.contains("gain ("), "{m}"),
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(h.stored("usb-a"), None);
}

#[test]
fn the_revision_six_methods_reach_only_the_five_image_controls() {
    let mut h = H::new("", with_brio);
    h.set_setting("camera.exposure.gain", SettingValue::Int(7), 0)
        .unwrap();
    h.set_control("", "sharpness", 50, 0).unwrap();
    assert!(matches!(
        h.set_control("", "gain", 1, 0),
        Err(IpcError::InvalidArgument(_))
    ));
    // The property: the image controls the device exposes, as before.
    assert_eq!(
        h.view().1,
        [
            entry("brightness", 128, false),
            entry("contrast", 128, false),
            entry("sharpness", 50, true),
        ]
    );
    // The same storage: the tree shows the value set through revision 6.
    assert!(h.setting(SHARP_ID).stored);
    // A closed reset forgets only the five, by name.
    h.reset_controls("", 100).unwrap();
    let st = h.stored("usb-a").unwrap();
    assert_eq!(
        st.values.into_iter().collect::<Vec<_>>(),
        [("gain".to_string(), 7)]
    );
    assert_eq!(st.reset_names.len(), 5);
    assert!(!st.reset_all);
    // The open resets the five it exposes, then applies the gain.
    h.activate(1000);
    assert_eq!(
        h.writes(),
        [
            w("usb-a", "brightness", 128),
            w("usb-a", "contrast", 128),
            w("usb-a", "sharpness", 128),
            w("usb-a", "gain", 7),
        ]
    );
    // They share the coalescing window.
    h.set_setting(SHARP_ID, SettingValue::Int(1), 2000).unwrap();
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    let now = h.at(2010);
    h.ctrl.handle(
        Command::SetCameraControl {
            camera: String::new(),
            name: "sharpness".into(),
            value: 2,
            reply: tx,
        },
        now,
    );
    assert!(rx.try_recv().is_err(), "held");
    h.tick(2050);
    assert!(matches!(rx.try_recv().unwrap(), Ok(())));
    assert_eq!(h.brio().value(SHARPNESS), 2);
}

#[test]
fn settings_are_saved_after_a_quiet_delay_and_flushed_by_any_other_save() {
    let mut h = H::new("", with_brio);
    let saved = |h: &H| {
        fs::read_to_string(&h.paths.state)
            .unwrap_or_default()
            .contains("sharpness")
    };
    h.set_setting(SHARP_ID, SettingValue::Int(10), 1000)
        .unwrap();
    assert!(!saved(&h));
    assert_eq!(h.ctrl.next_wakeup(h.at(1000)), Duration::from_millis(300));
    h.tick(1200);
    // Another change restarts the delay.
    h.set_setting(SHARP_ID, SettingValue::Int(11), 1200)
        .unwrap();
    h.tick(1499);
    assert!(!saved(&h));
    h.tick(1500);
    assert!(saved(&h));
    // Another state change saves at once, the settings with it.
    h.set_setting("camera.exposure.gain", SettingValue::Int(3), 2000)
        .unwrap();
    h.call(|r| Command::SetEffect("blur".into(), r), 2010)
        .unwrap();
    assert!(
        fs::read_to_string(&h.paths.state)
            .unwrap()
            .contains("gain = 3")
    );
    // Before a camera switch.
    h.activate(3000);
    h.set_setting("camera.exposure.gain", SettingValue::Int(4), 4000)
        .unwrap();
    h.call(|r| Command::SelectCamera("usb-b".into(), r), 4010)
        .unwrap();
    assert!(
        fs::read_to_string(&h.paths.state)
            .unwrap()
            .contains("gain = 4")
    );
    // At shutdown.
    h.set_setting(SHARP_ID, SettingValue::Int(2), 5000)
        .unwrap_or(());
    h.call(|r| Command::SelectCamera("usb-a".into(), r), 5100)
        .unwrap();
    h.set_setting("camera.exposure.gain", SettingValue::Int(5), 5200)
        .unwrap();
    h.ctrl.shutdown();
    assert!(
        fs::read_to_string(&h.paths.state)
            .unwrap()
            .contains("gain = 5")
    );
}

#[test]
fn the_watcher_runs_while_the_camera_is_open_and_an_event_publishes_the_delta() {
    let mut h = H::new("", with_brio);
    assert_eq!(h.world().watch, None, "never on a closed camera");
    h.activate(0);
    let (camera, ids) = h.world().watch.clone().expect("watching");
    assert_eq!(camera, "usb-a");
    assert!(ids.contains(&SHARPNESS) && ids.contains(&EXPOSURE) && ids.contains(&PAN_RESET));
    let generation = h.world().watch_generation;
    h.signals();
    // Another program sets manual exposure: the event, a read after the
    // settle delay, the delta.
    h.brio_mut(|d| d.change(AUTO_EXPOSURE, 1));
    let queries = h.world().control_queries;
    let now = h.at(1000);
    let b = h.brio();
    h.ctrl.handle(
        Command::ControlEvent(generation, ctrl_event(&b, AUTO_EXPOSURE, CTRL_CHANGE_VALUE)),
        now,
    );
    h.ctrl.handle(
        Command::ControlEvent(generation, ctrl_event(&b, EXPOSURE, CTRL_CHANGE_FLAGS)),
        now,
    );
    assert_eq!(h.world().control_queries, queries, "not read yet");
    h.tick(1049);
    assert!(h.signals().is_empty());
    h.tick(1050);
    assert_eq!(
        h.world().control_queries,
        queries + 1,
        "one read for the burst"
    );
    let sigs = h.signals();
    assert_eq!(sigs.len(), 1);
    assert_eq!(
        sigs[0].2,
        [
            change(MODE_ID, Some(s("manual")), false, ""),
            change(EXPOSURE_ID, Some(SettingValue::Int(1250)), false, ""),
        ]
    );
    // An event of an old watcher changes nothing.
    h.ctrl.handle(
        Command::ControlEvent(generation - 1, ctrl_event(&b, GAIN, CTRL_CHANGE_VALUE)),
        h.at(1100),
    );
    h.tick(1200);
    assert_eq!(h.world().control_queries, queries + 1);
    // A switch replaces it; the close stops it.
    h.call(|r| Command::SelectCamera("usb-b".into(), r), 2000)
        .unwrap();
    assert_eq!(
        h.world().watch.as_ref().map(|(c, _)| c.as_str()),
        Some("usb-b")
    );
    assert!(h.world().watch_generation > generation);
    h.consumers(0, 3000);
    h.tick(10_000);
    assert_eq!(h.state(), State::Idle);
    assert_eq!(h.world().watch, None);
    // A watcher that ended by itself is dropped.
    h.activate(20_000);
    let generation = h.world().watch_generation;
    h.ctrl.handle(
        Command::ControlEvent(generation, ControlEvent::Ended("No such device".into())),
        h.at(21_000),
    );
    assert_eq!(h.world().watch, None);
    // One that cannot start leaves the session streaming.
    h.consumers(0, 22_000);
    h.tick(30_000);
    h.world().fail_watch = Some("no events".into());
    h.activate(40_000);
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.world().watch, None);
}

#[test]
fn a_rename_of_the_open_camera_restarts_its_watcher() {
    let early = cam("/dev/video0", "Alpha Cam", Some("1111:0001"), "/dev/video0");
    let mut h = H::new("", |w| {
        w.cameras = vec![early.clone()];
        w.devices.insert("/dev/video0".into(), brio_device());
    });
    h.activate(0);
    let generation = h.world().watch_generation;
    assert_eq!(
        h.world().watch.as_ref().map(|(c, _)| c.as_str()),
        Some("/dev/video0")
    );
    {
        let mut w = h.world();
        w.cameras = vec![alpha()];
        let d = w.devices.remove("/dev/video0").unwrap();
        w.devices.insert("usb-a".into(), d);
    }
    h.rescan(1000);
    assert_eq!(h.state(), State::Active);
    assert_eq!(
        h.world().watch.as_ref().map(|(c, _)| c.as_str()),
        Some("usb-a")
    );
    assert!(h.world().watch_generation > generation);
}

#[test]
fn a_stored_keep_framerate_wins_over_the_configured_pin() {
    let config = "[camera]\nfixed_framerate = true\n";
    let mut h = H::new(config, with_brio);
    // Nothing stored: the config's pin, shown as what the open makes it.
    assert_eq!(
        h.setting(KEEP_ID).kind.value(),
        Some(SettingValue::Bool(true))
    );
    h.activate(0);
    assert_eq!(h.world().ingest_fixed_framerate, [true]);
    // Set while open and pinned: written and stored (the pin's guard puts
    // its value back at the close, which the fake does not model).
    h.writes();
    h.set_setting(KEEP_ID, SettingValue::Bool(false), 1000)
        .unwrap();
    assert_eq!(h.writes(), [w("usb-a", "exposure_dynamic_framerate", 1)]);
    assert_eq!(
        h.stored("usb-a").unwrap().values["exposure_dynamic_framerate"],
        1
    );
    // The next open is built without the pin, and applies the stored raw
    // value like any other.
    h.consumers(0, 2000);
    h.tick(10_000);
    h.activate(11_000);
    assert_eq!(h.world().ingest_fixed_framerate, [true, false]);
    assert_eq!(h.writes(), [w("usb-a", "exposure_dynamic_framerate", 1)]);
    // Forgotten again: the pin is back.
    h.reset_settings(&format!("setting:{KEEP_ID}"), 12_000)
        .unwrap();
    h.consumers(0, 13_000);
    h.tick(20_000);
    h.activate(21_000);
    assert_eq!(h.world().ingest_fixed_framerate, [true, false, true]);
    // A camera with nothing stored is pinned per the config, the other not.
    h.set_setting(KEEP_ID, SettingValue::Bool(true), 22_000)
        .unwrap();
    h.call(|r| Command::SelectCamera("usb-b".into(), r), 23_000)
        .unwrap();
    assert_eq!(h.world().ingest_fixed_framerate.last(), Some(&true));
    h.call(|r| Command::SelectCamera("usb-a".into(), r), 24_000)
        .unwrap();
    assert_eq!(h.world().ingest_fixed_framerate.last(), Some(&false));
}

#[test]
fn the_field_of_view_is_written_only_while_open_and_a_format_change_is_undone() {
    let mut h = H::new("", with_brio);
    h.set_setting(FOV_ID, s("78"), 0).unwrap();
    assert!(h.world().xu_writes.is_empty());
    assert_eq!(h.brio().fov.as_ref().unwrap().current, 2);
    assert_eq!(h.setting(FOV_ID).kind.value(), Some(s("78")));
    assert_eq!(h.stored("usb-a").unwrap().values["field_of_view"], 1);
    // The open applies it, the format checked before and after.
    let reads = h.world().format_reads;
    h.activate(1000);
    assert_eq!(h.world().xu_writes, [("usb-a".into(), "78".into())]);
    assert_eq!(h.brio().fov.as_ref().unwrap().current, 1);
    assert_eq!(h.world().format_reads, reads + 2);
    // A write that changes the format is put back and fails.
    let wide = obc_pipeline::CaptureFormat {
        width: 1280,
        height: 720,
        pixelformat: u32::from_le_bytes(*b"YUYV"),
    };
    h.brio_mut(|d| d.fov.as_mut().unwrap().changes_format = Some((0, wide)));
    match h.set_setting(FOV_ID, s("90"), 2000) {
        Err(IpcError::Failed(m)) => {
            assert!(
                m.starts_with("changing the field of view changed the camera's format"),
                "{m}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(
        h.world().xu_writes[1..],
        [
            ("usb-a".to_string(), "90".to_string()),
            ("usb-a".to_string(), "78".to_string())
        ]
    );
    assert_eq!(h.brio().fov.as_ref().unwrap().current, 1);
    assert_eq!(h.stored("usb-a").unwrap().values["field_of_view"], 1);
    // An unknown byte greys it.
    h.brio_mut(|d| d.fov.as_mut().unwrap().current = 9);
    h.rescan(3000);
    let f = h.setting(FOV_ID);
    assert!(!f.enabled);
    assert!(f.disabled_reason.contains("field of view"));
}

#[test]
fn the_serial_moves_with_the_structure_and_the_revision_with_every_change() {
    let mut h = H::new("", with_brio);
    let ids = |h: &H| (h.snap().settings_revision, h.snap().settings_schema_serial);
    assert_eq!(ids(&h), (Some(1), Some(1)));
    // A value: the revision only, its change announced.
    h.set_setting("camera.exposure.gain", SettingValue::Int(3), 0)
        .unwrap();
    assert_eq!(ids(&h), (Some(2), Some(1)));
    let (changes, _) = h.drain();
    assert_eq!(h.signals().len(), 1);
    assert!(changes.contains(&Change::SettingsRevision(2)));
    assert!(
        !changes
            .iter()
            .any(|c| matches!(c, Change::SettingsSchemaSerial(_)))
    );
    // Nothing changed: nothing announced.
    h.rescan(100);
    assert_eq!(ids(&h), (Some(2), Some(1)));
    assert!(h.signals().is_empty());
    // ActiveCamera changes: structural, an empty change list.
    h.call(|r| Command::SelectCamera("usb-b".into(), r), 200)
        .unwrap();
    assert_eq!(ids(&h), (Some(3), Some(2)));
    assert_eq!(h.signals(), [(3, 2, Vec::new())]);
    assert_eq!(h.get_settings("", 200).unwrap().camera, "usb-b");
    // A range the device changed: structural.
    h.world().controls.get_mut("usb-b").unwrap()[0].max = 9;
    h.rescan(300);
    assert_eq!(ids(&h), (Some(4), Some(3)));
}

// ---- the review fixes of the settings channel ----

const FOCUS_AUTO: u32 = 0x009a_090c;
const FOCUS_ABS: u32 = 0x009a_090a;
const FOCUS_AUTO_ID: &str = "camera.focus.focus_automatic_continuous";
const FOCUS_ID: &str = "camera.focus.focus_absolute";

/// The fake MX Brio with continuous autofocus and the focus it greys
/// (after the exposure controls in the tree).
fn with_focus_brio(w: &mut World) {
    with_brio(w);
    let d = w.devices.get_mut("usb-a").unwrap();
    d.controls.push(node_control(
        FOCUS_AUTO,
        "Focus, Automatic Continuous",
        obc_pipeline::controls::CTRL_TYPE_BOOLEAN,
        (0, 1, 1),
        1,
        1,
        0,
    ));
    d.controls.push(node_control(
        FOCUS_ABS,
        "Focus, Absolute",
        CTRL_TYPE_INTEGER,
        (0, 255, 1),
        0,
        40,
        CTRL_FLAG_INACTIVE,
    ));
    d.links.push((FOCUS_ABS, FOCUS_AUTO, vec![0]));
}

/// Stores manual exposure, an exposure time, manual focus and a focus
/// while the camera is closed (nothing written).
fn store_manual_exposure_and_focus(h: &mut H) {
    for (id, v) in [
        (MODE_ID, s("manual")),
        (EXPOSURE_ID, SettingValue::Int(400)),
        (FOCUS_AUTO_ID, SettingValue::Bool(false)),
        (FOCUS_ID, SettingValue::Int(90)),
    ] {
        h.set_setting(id, v, 0).unwrap();
    }
    assert!(h.writes().is_empty());
}

#[test]
fn a_stored_dependent_a_read_left_out_before_the_masters_is_applied_after_them() {
    let mut h = H::new("", with_focus_brio);
    store_manual_exposure_and_focus(&mut h);
    // The first read at the open cannot read the exposure time (under
    // automatic exposure); after the masters' writes it can. The read
    // after them has one entry more, before the focus.
    let queries = h.world().control_queries;
    h.world().next_read_failures =
        vec![vec!["exposure_time_absolute".to_string()], Vec::new()].into();
    h.activate(1000);
    assert_eq!(
        h.writes(),
        [
            w("usb-a", "auto_exposure", 1),
            w("usb-a", "focus_automatic_continuous", 0),
            w("usb-a", "exposure_time_absolute", 400),
            w("usb-a", "focus_absolute", 90),
        ]
    );
    assert_eq!(h.brio().value(EXPOSURE), 400);
    assert_eq!(h.brio().value(FOCUS_ABS), 90);
    // The open's read, the read after the masters, the first frame's.
    assert_eq!(h.world().control_queries, queries + 3);
    assert_eq!(h.state(), State::Active);
}

#[test]
fn a_stored_dependent_unreadable_after_the_masters_is_kept_and_nothing_else_is_written() {
    let mut h = H::new("", with_focus_brio);
    store_manual_exposure_and_focus(&mut h);
    // Readable at the open; the read after the masters' writes cannot read
    // the exposure time: one entry fewer, before the focus.
    h.world().next_read_failures = vec![
        Vec::new(),
        vec!["exposure_time_absolute".to_string()],
        Vec::new(),
    ]
    .into();
    h.activate(1000);
    assert_eq!(
        h.writes(),
        [
            w("usb-a", "auto_exposure", 1),
            w("usb-a", "focus_automatic_continuous", 0),
            w("usb-a", "focus_absolute", 90),
        ]
    );
    assert_eq!(h.brio().value(EXPOSURE), 1250, "not written");
    assert_eq!(h.state(), State::Active);
    // Kept for the camera, and logged as unreadable (not silently).
    assert_eq!(
        h.stored("usb-a").unwrap().values["exposure_time_absolute"],
        400
    );
    assert!(
        h.ctrl
            .controls_skipped
            .contains(&("usb-a".to_string(), "exposure_time_absolute".to_string()))
    );
}

#[test]
fn a_reset_supersedes_the_held_writes_in_its_scope() {
    let mut h = H::new("", with_brio);
    h.activate(0);
    let gain = "camera.exposure.gain";
    h.set_setting(SHARP_ID, SettingValue::Int(10), 1000)
        .unwrap();
    h.set_setting(gain, SettingValue::Int(1), 1000).unwrap();
    let mut sharp = h.send_set("", SHARP_ID, Some(SettingValue::Int(20)), 1010);
    let mut held_gain = h.send_set("", gain, Some(SettingValue::Int(2)), 1010);
    assert!(sharp.try_recv().is_err() && held_gain.try_recv().is_err());
    h.writes();
    // One setting: its held write is answered and dropped, the other stays.
    h.reset_settings(&format!("setting:{SHARP_ID}"), 1020)
        .unwrap();
    assert!(matches!(sharp.try_recv().unwrap(), Ok(())));
    assert!(held_gain.try_recv().is_err(), "another setting stays held");
    assert_eq!(h.writes(), [w("usb-a", "sharpness", 128)]);
    h.tick(1050);
    assert!(matches!(held_gain.try_recv().unwrap(), Ok(())));
    assert_eq!(h.writes(), [w("usb-a", "gain", 2)]);
    assert_eq!(h.brio().value(SHARPNESS), 128);
    let st = h.stored("usb-a").unwrap();
    assert_eq!(
        st.values.into_iter().collect::<Vec<_>>(),
        [("gain".to_string(), 2)]
    );
    // The whole camera, then the revision-6 reset of the five: after the
    // window nothing is written or stored again.
    for (k, whole) in [(0u64, true), (1, false)] {
        let t = 2000 + k * 1000;
        h.set_setting(SHARP_ID, SettingValue::Int(30), t).unwrap();
        let mut held = h.send_set("", SHARP_ID, Some(SettingValue::Int(31)), t + 10);
        assert!(held.try_recv().is_err());
        if whole {
            h.reset_settings("camera", t + 20).unwrap();
        } else {
            h.reset_controls("", t + 20).unwrap();
        }
        assert!(matches!(held.try_recv().unwrap(), Ok(())));
        h.writes();
        h.tick(t + 100);
        assert!(h.writes().is_empty(), "nothing written after the reset");
        assert_eq!(h.brio().value(SHARPNESS), 128);
        assert!(
            h.stored("usb-a")
                .is_none_or(|st| !st.values.contains_key("sharpness")),
            "nothing stored again"
        );
    }
}

#[test]
fn a_held_write_whose_camera_closes_or_switches_is_stored_and_never_written() {
    let mut h = H::new("[lifecycle]\nidle_grace_s = 0.0\n", with_brio);
    h.activate(0);
    h.writes();
    // The camera closes inside the window.
    h.set_setting(SHARP_ID, SettingValue::Int(10), 1000)
        .unwrap();
    let mut held = h.send_set("", SHARP_ID, Some(SettingValue::Int(20)), 1010);
    assert!(held.try_recv().is_err());
    h.consumers(0, 1020);
    assert_eq!(h.state(), State::Idle);
    h.tick(1050);
    assert!(matches!(held.try_recv().unwrap(), Ok(())));
    assert_eq!(h.writes(), [w("usb-a", "sharpness", 10)]);
    assert_eq!(h.brio().value(SHARPNESS), 10, "never written closed");
    assert_eq!(h.stored("usb-a").unwrap().values["sharpness"], 20);
    // The session switches to another camera inside the window.
    h.activate(2000);
    assert_eq!(h.writes(), [w("usb-a", "sharpness", 20)]);
    h.set_setting(SHARP_ID, SettingValue::Int(30), 3000)
        .unwrap();
    let mut held = h.send_set("", SHARP_ID, Some(SettingValue::Int(40)), 3010);
    assert!(held.try_recv().is_err());
    h.call(|r| Command::SelectCamera("usb-b".into(), r), 3020)
        .unwrap();
    h.writes();
    h.tick(3050);
    assert!(matches!(held.try_recv().unwrap(), Ok(())));
    assert!(h.writes().is_empty(), "not on the other camera either");
    assert_eq!(h.device("usb-b", "sharpness"), 5);
    assert_eq!(h.brio().value(SHARPNESS), 30);
    assert_eq!(h.stored("usb-a").unwrap().values["sharpness"], 40);
    assert_eq!(h.stored("usb-b"), None);
}

#[test]
fn every_held_reply_is_answered_at_shutdown_without_a_step_each() {
    let mut h = H::new("", with_brio);
    h.activate(0);
    let gain = "camera.exposure.gain";
    h.set_setting(SHARP_ID, SettingValue::Int(10), 1000)
        .unwrap();
    h.set_setting(gain, SettingValue::Int(1), 1000).unwrap();
    let mut a = h.send_set("", SHARP_ID, Some(SettingValue::Int(20)), 1010);
    let mut b = h.send_set("", gain, Some(SettingValue::Int(2)), 1010);
    assert!(a.try_recv().is_err() && b.try_recv().is_err());
    h.writes();
    h.signals();
    h.ctrl.shutdown();
    assert!(matches!(a.try_recv().unwrap(), Ok(())));
    assert!(matches!(b.try_recv().unwrap(), Ok(())));
    // Written while the camera was still open, and saved.
    assert_eq!(
        h.writes(),
        [w("usb-a", "sharpness", 20), w("usb-a", "gain", 2)]
    );
    let st = h.stored("usb-a").unwrap();
    assert_eq!((st.values["sharpness"], st.values["gain"]), (20, 2));
    // No lifecycle step (and so no publish) per held request.
    assert!(h.signals().is_empty());
}

#[test]
fn a_settings_change_the_state_file_cannot_take_fails_at_once() {
    // A damaged state file is never overwritten: the change is in force
    // (stored, shown), the call says it is not saved.
    let mut h = H::new("", with_brio);
    let damaged = "enabled = \"false\"\n";
    fs::create_dir_all(h.paths.state.parent().unwrap()).unwrap();
    fs::write(&h.paths.state, damaged).unwrap();
    h.restart();
    h.world().devices.insert("usb-a".into(), brio_device());
    let not_saved = |r: Result<(), IpcError>| match r {
        Err(IpcError::Failed(m)) => {
            assert!(m.contains("in force") && m.contains("SetEnabled"), "{m}");
        }
        other => panic!("expected Failed, got {other:?}"),
    };
    not_saved(h.set_setting(SHARP_ID, SettingValue::Int(10), 0));
    assert!(h.setting(SHARP_ID).stored);
    not_saved(h.set_control("", "contrast", 3, 0));
    not_saved(h.reset_settings(&format!("setting:{SHARP_ID}"), 0));
    // Nothing changed: nothing to save, no failure.
    h.set_setting("camera.exposure.gain", SettingValue::Int(0), 0)
        .unwrap_err();
    assert_eq!(fs::read_to_string(&h.paths.state).unwrap(), damaged);

    // A state file that cannot be written: the first change is saved
    // behind (it fails: logged, shown in Error); the next ones are saved at
    // once while it fails, and Failed; once it works again, Ok and the
    // error is gone.
    let mut h = H::new("", with_brio);
    let parent = h.paths.state.parent().unwrap().to_path_buf();
    fs::write(&parent, b"not a dir").unwrap();
    h.set_setting(SHARP_ID, SettingValue::Int(10), 1000)
        .unwrap();
    assert_eq!(h.snap().error, "");
    h.tick(1300);
    let e = h.snap().error;
    assert!(e.contains("will not survive a restart"), "{e}");
    match h.set_setting(SHARP_ID, SettingValue::Int(11), 1400) {
        Err(IpcError::Failed(m)) => assert!(m.contains("will not survive a restart"), "{m}"),
        other => panic!("expected Failed, got {other:?}"),
    }
    fs::remove_file(&parent).unwrap();
    h.set_setting(SHARP_ID, SettingValue::Int(12), 1500)
        .unwrap();
    assert_eq!(h.snap().error, "");
    assert!(
        fs::read_to_string(&h.paths.state)
            .unwrap()
            .contains("sharpness = 12")
    );
    // Back to the delayed save.
    h.set_setting(SHARP_ID, SettingValue::Int(13), 1600)
        .unwrap();
    assert!(
        fs::read_to_string(&h.paths.state)
            .unwrap()
            .contains("sharpness = 12")
    );
}

#[test]
fn a_reset_keeps_the_frame_rate_pin() {
    let config = "[camera]\nfixed_framerate = true\n";
    // A camera whose default is the dynamic frame rate, pinned to 0 by
    // the ingest (the fake leaves the value as the pin made it).
    let mut h = H::new(config, |w| {
        with_brio(w);
        let d = w.devices.get_mut("usb-a").unwrap();
        let c = d
            .controls
            .iter_mut()
            .find(|c| c.id == crate::backend::fake::cid::DYNAMIC_FPS)
            .unwrap();
        c.info.default_value = 1;
        c.value = CtrlValue::Value(0);
    });
    let dynamic = |h: &H| h.brio().value(crate::backend::fake::cid::DYNAMIC_FPS);
    let wrote_it = |writes: &[(String, String, i32)]| {
        writes
            .iter()
            .any(|(_, n, _)| n == "exposure_dynamic_framerate")
    };
    h.activate(0);
    assert_eq!(h.world().ingest_fixed_framerate, [true]);
    h.writes();
    // Open: a whole reset leaves the pin.
    h.reset_settings("camera", 1000).unwrap();
    let writes = h.writes();
    assert!(!wrote_it(&writes), "{writes:?}");
    assert_eq!(dynamic(&h), 0);
    // Changed over the pin, then reset: back to the pin's 0, forgotten.
    h.set_setting(KEEP_ID, SettingValue::Bool(false), 2000)
        .unwrap();
    assert_eq!(dynamic(&h), 1);
    h.writes();
    h.reset_settings(&format!("setting:{KEEP_ID}"), 2100)
        .unwrap();
    assert_eq!(h.writes(), [w("usb-a", "exposure_dynamic_framerate", 0)]);
    assert_eq!(h.stored("usb-a"), None);
    // A whole reset left for the open: shown as the pin makes it, and the
    // open does not write the default over the pin.
    h.consumers(0, 3000);
    h.tick(10_000);
    assert_eq!(h.state(), State::Idle);
    h.reset_settings("camera", 10_000).unwrap();
    assert!(h.stored("usb-a").unwrap().reset_all);
    assert_eq!(
        h.setting(KEEP_ID).kind.value(),
        Some(SettingValue::Bool(true))
    );
    h.activate(11_000);
    assert_eq!(h.world().ingest_fixed_framerate.last(), Some(&true));
    let writes = h.writes();
    assert!(!writes.is_empty(), "the other defaults");
    assert!(!wrote_it(&writes), "{writes:?}");
    assert_eq!(dynamic(&h), 0);
    assert_eq!(h.stored("usb-a"), None);
}

#[test]
fn a_set_on_the_open_camera_reads_back_only_what_depends_on_it() {
    let mut h = H::new("", with_brio);
    h.activate(0);
    h.writes();
    h.signals();
    let counts = |h: &H| {
        let w = h.world();
        (
            w.control_opens,
            w.control_queries,
            w.control_reads,
            w.control_info_reads,
        )
    };
    // A plain control: one open, the write read back, no full read.
    let (o, q, r, i) = counts(&h);
    h.set_setting(SHARP_ID, SettingValue::Int(10), 1000)
        .unwrap();
    assert_eq!(counts(&h), (o + 1, q, r + 1, i));
    // A master: the dependent's flags and value as well, still no full
    // read; the flip comes in the same signal.
    let (o, q, r, i) = counts(&h);
    h.set_setting(MODE_ID, s("manual"), 1100).unwrap();
    assert_eq!(counts(&h), (o + 1, q, r + 2, i + 1));
    let sigs = h.signals();
    assert_eq!(sigs.len(), 2, "{sigs:?}");
    assert_eq!(
        sigs[1].2,
        [
            change(MODE_ID, Some(s("manual")), true, ""),
            change(EXPOSURE_ID, Some(SettingValue::Int(1250)), false, ""),
        ]
    );
    // A refused value is refused against the published tree, without
    // touching the device.
    let before = counts(&h);
    assert!(matches!(
        h.set_setting(SHARP_ID, SettingValue::Int(999), 1200),
        Err(IpcError::InvalidArgument(_))
    ));
    assert_eq!(counts(&h), before);
    // An action reads every control again (it may change any).
    let (_, q, _, _) = counts(&h);
    h.set_setting("camera.focus.pan_reset", SettingValue::Bool(true), 1300)
        .unwrap();
    assert_eq!(counts(&h).1, q + 1);
    // So does a control the device says changes others when the daemon
    // knows none of them.
    h.brio_mut(|d| {
        d.controls
            .iter_mut()
            .find(|c| c.id == GAIN)
            .unwrap()
            .info
            .flags |= CTRL_FLAG_UPDATE;
    });
    h.rescan(1400);
    let (_, q, _, _) = counts(&h);
    h.set_setting("camera.exposure.gain", SettingValue::Int(3), 1500)
        .unwrap();
    assert_eq!(counts(&h).1, q + 1);

    // The watcher's events of the daemon's own writes report what was read
    // back: no read again. Another program's change is read.
    let generation = h.world().watch_generation;
    let b = h.brio();
    let (_, q, _, _) = counts(&h);
    for (id, changes) in [
        (SHARPNESS, CTRL_CHANGE_VALUE),
        (AUTO_EXPOSURE, CTRL_CHANGE_VALUE),
        (EXPOSURE, CTRL_CHANGE_FLAGS),
    ] {
        h.ctrl.handle(
            Command::ControlEvent(generation, ctrl_event(&b, id, changes)),
            h.at(1600),
        );
    }
    h.tick(1700);
    assert_eq!(counts(&h).1, q, "nothing new: no read");
    h.brio_mut(|d| d.change(SHARPNESS, 77));
    let b = h.brio();
    h.ctrl.handle(
        Command::ControlEvent(generation, ctrl_event(&b, SHARPNESS, CTRL_CHANGE_VALUE)),
        h.at(1800),
    );
    h.tick(1850);
    assert_eq!(counts(&h).1, q + 1);
    assert_eq!(
        h.setting(SHARP_ID).kind.value(),
        Some(SettingValue::Int(77))
    );
}

#[test]
fn a_held_write_follows_a_rename_of_the_open_camera() {
    let early = cam("/dev/video0", "Alpha Cam", Some("1111:0001"), "/dev/video0");
    let mut h = H::new("", |w| {
        w.cameras = vec![early.clone()];
        w.devices.insert("/dev/video0".into(), brio_device());
    });
    h.activate(0);
    h.set_setting(SHARP_ID, SettingValue::Int(10), 1000)
        .unwrap();
    let mut held = h.send_set("", SHARP_ID, Some(SettingValue::Int(20)), 1010);
    assert!(held.try_recv().is_err());
    assert_eq!(h.writes(), [w("/dev/video0", "sharpness", 10)]);
    {
        let mut w = h.world();
        w.cameras = vec![alpha()];
        let d = w.devices.remove("/dev/video0").unwrap();
        w.devices.insert("usb-a".into(), d);
    }
    h.rescan(1020);
    assert_eq!(h.snap().active_camera, "usb-a");
    assert!(held.try_recv().is_err(), "still held");
    h.tick(1050);
    assert!(matches!(held.try_recv().unwrap(), Ok(())));
    assert_eq!(h.writes(), [w("usb-a", "sharpness", 20)]);
    assert_eq!(h.brio().value(SHARPNESS), 20);
    assert_eq!(h.stored("usb-a").unwrap().values["sharpness"], 20);
}

// ---- the global settings: the video effects (revision 8) ----

use crate::settings::effects::{BLUR_STRENGTH as BLUR_ID, MODE as EFFECT_ID, REASON_CHOOSE_BLUR};
use obc_core::modules::legacy::DENOISE as DENOISE_ID;

/// The Maxine module's reason of a stage whose SDK features are missing.
const REASON_NOT_INSTALLED: &str = "Not installed";
use obc_pipeline::effect::{Availability, StageId};

/// The stages the Maxine backend reports.
const STAGES: [StageId; 4] = [
    StageId::BACKGROUND,
    StageId::DENOISE,
    StageId::RELIGHT,
    StageId::ENHANCE,
];

/// Every stage installed but `missing`, missing for `why`.
fn stage_missing(missing: &StageId, why: &str) -> Vec<(StageId, Availability)> {
    STAGES
        .into_iter()
        .map(|s| {
            let a = if s == *missing {
                Availability::NotInstalled(why.into())
            } else {
                Availability::Installed
            };
            (s, a)
        })
        .collect()
}

/// Every stage installed but noise removal, missing for `why`.
fn denoise_missing(why: &str) -> Vec<(StageId, Availability)> {
    stage_missing(&StageId::DENOISE, why)
}

fn all_installed() -> Vec<(StageId, Availability)> {
    STAGES
        .into_iter()
        .map(|s| (s, Availability::Installed))
        .collect()
}

/// The ids of the settings each signal carries.
fn signal_ids(signals: &[(u32, u32, Vec<SettingChange>)]) -> Vec<Vec<String>> {
    signals
        .iter()
        .map(|(_, _, c)| c.iter().map(|c| c.id.clone()).collect())
        .collect()
}

/// The state file as saved now (no delayed save flushed).
fn saved_now(h: &H) -> RuntimeState {
    let (s, warn) = RuntimeState::load(&h.paths.state);
    assert_eq!(warn, None);
    legacy_view(s)
}

#[test]
fn set_setting_on_a_global_id_is_the_effect_method_it_stands_for() {
    let mut h = H::new("", |_| {});
    h.activate(0);
    h.signals();
    h.set_setting(EFFECT_ID, s("blur"), 1000).unwrap();
    assert_eq!(h.snap().effect, "blur");
    assert_eq!(h.applied(), Some(EffectMode::Blur), "applied live");
    // Saved at once, as SetEffect saves.
    assert_eq!(saved_now(&h).effect, Some(EffectMode::Blur));
    assert_eq!(effect_changes(&h.drain().0), ["effect=blur"]);
    // One signal: the mode and the strength it enables.
    let sig = h.signals();
    assert_eq!(signal_ids(&sig), [vec![EFFECT_ID, BLUR_ID]]);
    assert_eq!(
        sig[0].2,
        [
            change(EFFECT_ID, Some(s("blur")), true, ""),
            change(BLUR_ID, Some(SettingValue::Int(50)), false, "")
        ]
    );
    h.set_setting(BLUR_ID, SettingValue::Int(30), 1100).unwrap();
    assert_eq!(h.snap().blur_strength, 0.3);
    assert_eq!(saved_now(&h).blur_strength, Some(0.3));
    assert_eq!(effect_changes(&h.drain().0), ["strength=0.3"]);
    assert_eq!(
        h.signals()[0].2,
        [change(BLUR_ID, Some(SettingValue::Int(30)), true, "")]
    );
    assert_eq!(h.world().effect.as_ref().unwrap().blur_strength, 0.3);
    // Not a device write: no coalescing, every value answered at once.
    for (i, v) in [31, 32, 33].into_iter().enumerate() {
        h.set_setting(BLUR_ID, SettingValue::Int(v), 1101 + i as u64)
            .unwrap();
    }
    assert_eq!(h.snap().blur_strength, 0.33);
    h.set_setting(DENOISE_ID, SettingValue::Bool(true), 1200)
        .unwrap();
    assert!(h.snap().denoise);
    assert_eq!(h.denoise_applied(), Some(true));
    assert_eq!(saved_now(&h).denoise, Some(true));
    // The camera option is ignored for a global id.
    let mut r = h.send_set(
        "usb-nope",
        DENOISE_ID,
        Some(SettingValue::Bool(false)),
        1300,
    );
    assert!(matches!(r.try_recv().unwrap(), Ok(())));
    assert!(!h.snap().denoise);
    assert_eq!(h.state(), State::Active, "never restarted");
    assert_eq!(h.world().session_starts, 1);
}

#[test]
fn global_writes_refuse_as_the_methods_do_in_the_order_of_the_checks() {
    let mut h = H::new("", |_| {});
    // Replace without an image: the SetEffect error, word for word.
    let old = h.set_effect("replace", 0).unwrap_err();
    let new = h.set_setting(EFFECT_ID, s("replace"), 0).unwrap_err();
    assert!(matches!(new, IpcError::InvalidArgument(_)), "{new:?}");
    assert_eq!(format!("{new:?}"), format!("{old:?}"));
    assert_eq!(h.snap().effect, "none");
    assert!(matches!(
        h.set_setting("effect.background.nope", s("x"), 0),
        Err(IpcError::UnknownSetting(_))
    ));
    // The strength is disabled until Blur is chosen, whatever the value.
    for v in [SettingValue::Int(40), SettingValue::Int(500), s("x")] {
        assert!(matches!(
            h.set_setting(BLUR_ID, v, 0),
            Err(IpcError::Disabled(m)) if m == REASON_CHOOSE_BLUR
        ));
    }
    assert!(matches!(
        h.set_setting(EFFECT_ID, s("Blur"), 0),
        Err(IpcError::InvalidArgument(_))
    ));
    h.set_effect("blur", 0).unwrap();
    for v in [SettingValue::Int(101), SettingValue::Bool(true)] {
        assert!(matches!(
            h.set_setting(BLUR_ID, v, 0),
            Err(IpcError::InvalidArgument(_))
        ));
    }
    let mut r = h.send_set("", BLUR_ID, None, 0);
    assert!(matches!(
        r.try_recv().unwrap(),
        Err(IpcError::InvalidArgument(_))
    ));
    assert_eq!(h.snap().blur_strength, 0.5, "nothing refused changed");
}

#[test]
fn a_global_write_clears_a_fault_as_the_methods_do() {
    let mut h = H::new("[effect]\nmode = \"blur\"\n", |_| {});
    h.activate(0);
    h.world().failure = Some(Failure::Output("output pipeline failed: boom".into()));
    h.tick(1000);
    assert_eq!(h.state(), State::Error);
    h.set_setting(DENOISE_ID, SettingValue::Bool(true), 2000)
        .unwrap();
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.world().session_starts, 2);
    assert_eq!(h.snap().error, "");
}

#[test]
fn the_effect_methods_and_a_reload_announce_the_global_settings() {
    let mut h = H::new("", |_| {});
    h.signals();
    h.set_effect("blur", 0).unwrap();
    h.set_strength(0.7, 10).unwrap();
    h.set_denoise(true, 20).unwrap();
    let sig = h.signals();
    assert_eq!(
        signal_ids(&sig),
        [vec![EFFECT_ID, BLUR_ID], vec![BLUR_ID], vec![DENOISE_ID]]
    );
    assert_eq!(
        sig[1].2,
        [change(BLUR_ID, Some(SettingValue::Int(70)), true, "")]
    );
    // Consecutive revisions, one per operation, no structural change.
    let revs: Vec<u32> = sig.iter().map(|(r, _, _)| *r).collect();
    assert_eq!(revs, [revs[0], revs[0] + 1, revs[0] + 2]);
    assert!(sig.iter().all(|(_, serial, _)| *serial == sig[0].1));
    assert_eq!(h.snap().settings_revision, Some(revs[2]));
    // The same value again: nothing.
    h.set_denoise(true, 30).unwrap();
    assert!(h.signals().is_empty());
    // The background image is outside the tree: no signal.
    let image = h.image();
    h.set_image(&image, 40).unwrap();
    assert!(h.signals().is_empty());
    // A reload that changes a default is structural.
    let serial = h.snap().settings_schema_serial.unwrap();
    h.reload("[effect]\nblur_strength = 0.2\n", 50).unwrap();
    let sig = h.signals();
    assert_eq!(sig.len(), 1);
    assert_eq!(sig[0].1, serial + 1);
    assert!(sig[0].2.is_empty(), "structural: {sig:?}");
    assert_eq!(
        h.setting(BLUR_ID).kind.default_value(),
        Some(SettingValue::Int(20))
    );
}

#[test]
fn a_global_reset_puts_the_configurations_value_back_and_camera_reset_leaves_it() {
    let mut h = H::new("[effect]\nblur_strength = 0.4\n", with_brio);
    h.activate(0);
    h.set_effect("blur", 0).unwrap();
    h.set_strength(0.9, 0).unwrap();
    h.set_denoise(true, 0).unwrap();
    h.signals();
    // The camera scope never touches them.
    h.reset_settings("camera", 1000).unwrap();
    let st = saved_now(&h);
    assert_eq!(
        (st.effect, st.blur_strength, st.denoise),
        (Some(EffectMode::Blur), Some(0.9), Some(true))
    );
    assert!(h.setting(BLUR_ID).stored);
    h.signals();
    // One setting.
    h.reset_settings(&format!("setting:{BLUR_ID}"), 1100)
        .unwrap();
    assert_eq!(h.snap().blur_strength, 0.4);
    assert_eq!(saved_now(&h).blur_strength, None);
    assert_eq!(
        h.world().effect.as_ref().unwrap().blur_strength,
        0.4,
        "live"
    );
    assert_eq!(
        h.signals()[0].2,
        [change(BLUR_ID, Some(SettingValue::Int(40)), false, "")]
    );
    // A group, with the camera option ignored.
    let res = h.call(
        |reply| Command::ResetSettings {
            camera: "usb-nope".into(),
            scope: "group:background".into(),
            reply,
        },
        1200,
    );
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(h.snap().effect, "none");
    assert_eq!(h.applied(), Some(EffectMode::None));
    assert_eq!(saved_now(&h).effect, None);
    assert!(h.snap().denoise, "another group");
    assert_eq!(
        signal_ids(&h.signals()),
        [vec![EFFECT_ID, BLUR_ID]],
        "one signal for the reset"
    );
    h.reset_settings("group:video", 1300).unwrap();
    assert!(!h.snap().denoise);
    assert_eq!(saved_now(&h).denoise, None);
    assert_eq!(signal_ids(&h.signals()), [vec![DENOISE_ID]]);
    // Nothing stored: still fine, nothing announced.
    h.reset_settings("group:video", 1400).unwrap();
    assert!(h.signals().is_empty());
    assert!(matches!(
        h.reset_settings("setting:effect.video.nope", 1500),
        Err(IpcError::UnknownSetting(_))
    ));
    assert_eq!(h.state(), State::Active);
}

#[test]
fn global_settings_are_there_without_a_camera_and_work() {
    let mut h = H::new("", |w| w.cameras.clear());
    let tree = h.get_settings("", 0).unwrap();
    assert_eq!(tree.camera, "");
    let ids: Vec<&str> = tree.groups.iter().map(|g| g.id.as_str()).collect();
    assert_eq!(ids, ["background", "video", "lighting"]);
    h.set_setting(EFFECT_ID, s("blur"), 0).unwrap();
    assert_eq!(h.snap().effect, "blur");
    assert_eq!(
        h.setting(EFFECT_ID).kind.value(),
        Some(s("blur")),
        "the tree follows"
    );
    h.reset_settings("group:background", 10).unwrap();
    assert_eq!(h.snap().effect, "none");
    // A camera setting still needs a camera.
    assert!(matches!(
        h.set_setting(SHARP_ID, SettingValue::Int(1), 20),
        Err(IpcError::NoSuchCamera(_) | IpcError::Unavailable(_))
    ));
}

#[test]
fn noise_removal_not_installed_is_never_requested_and_raises_no_error() {
    let why = "no Denoise model for this GPU (sm_120) in /opt/fake/maxine/lib/models";
    let mut h = H::new("[effect]\ndenoise = true\n", |w| {
        w.features = Some(denoise_missing(why));
        // Were it requested, it would fail and show in Error.
        w.denoise_fail = Some(NO_DENOISE.into());
    });
    let d = h.setting(DENOISE_ID);
    assert!(!d.enabled);
    assert_eq!(d.disabled_reason, REASON_NOT_INSTALLED);
    assert_eq!(d.kind.value(), Some(SettingValue::Bool(false)), "what runs");
    assert!(h.snap().denoise, "the request stays");
    h.activate(0);
    h.tick(500);
    h.tick(750);
    assert_eq!(h.denoise_applied(), Some(false));
    assert_eq!(h.snap().error, "");
    assert_no_denoise_text(&mut h);
    assert!(matches!(
        h.set_setting(DENOISE_ID, SettingValue::Bool(true), 800),
        Err(IpcError::Disabled(m)) if m == REASON_NOT_INSTALLED
    ));
    // The method still stores the choice; it is not requested either.
    h.set_denoise(false, 900).unwrap();
    h.set_denoise(true, 950).unwrap();
    assert_eq!(saved_now(&h).denoise, Some(true));
    assert!(h.world().effect_sets.iter().all(|e| !e.denoise()));
    assert_eq!(h.snap().error, "");
    // Installed (found on a reload): it turns on by itself, and a real
    // failure is reported as before.
    h.world().features = Some(all_installed());
    h.signals();
    h.reload("[effect]\ndenoise = true\n", 1000).unwrap();
    let d = h.setting(DENOISE_ID);
    assert!(d.enabled && d.kind.value() == Some(SettingValue::Bool(true)));
    assert_eq!(
        h.signals()[0].2,
        [change(DENOISE_ID, Some(SettingValue::Bool(true)), true, "")]
    );
    assert_eq!(h.denoise_applied(), Some(true));
    h.tick(1250);
    assert_eq!(h.snap().error, denoise_error(NO_DENOISE));
}

#[test]
fn unknown_availability_requests_noise_removal_as_before() {
    let mut h = H::new("[effect]\ndenoise = true\n", |w| {
        w.denoise_fail = Some(NO_DENOISE.into())
    });
    assert!(h.world().features.is_none());
    let d = h.setting(DENOISE_ID);
    assert!(d.enabled && d.kind.value() == Some(SettingValue::Bool(true)));
    h.activate(0);
    h.tick(500);
    assert_eq!(h.denoise_applied(), Some(true));
    assert_eq!(h.snap().error, denoise_error(NO_DENOISE));
}

#[test]
fn the_compute_capability_is_read_once_with_a_session_never_at_idle() {
    let mut h = H::new(
        "[effect]\ndenoise = true\n[lifecycle]\nidle_grace_s = 0.0\n",
        |w| {
            w.features = Some(all_installed());
            // This GPU's models are missing.
            w.features_with_cc = Some(denoise_missing("no Denoise model for this GPU (sm_86)"));
            w.compute_capability = Some((8, 6));
        },
    );
    // At start: the filesystem only, with no compute capability.
    assert_eq!(h.world().feature_probes, [None]);
    assert!(h.setting(DENOISE_ID).enabled);
    h.reload(
        "[effect]\ndenoise = true\n[lifecycle]\nidle_grace_s = 0.0\n",
        10,
    )
    .unwrap();
    assert_eq!(h.world().feature_probes, [None, None]);
    assert_eq!(h.world().cc_reads, 0, "no GPU call at idle");
    // A session: read once, the features read again for this GPU, and the
    // stage taken off the running session.
    h.signals();
    h.activate(100);
    assert_eq!(h.world().cc_reads, 1);
    assert_eq!(h.world().feature_probes.last(), Some(&Some((8, 6))));
    let d = h.setting(DENOISE_ID);
    assert_eq!(d.disabled_reason, REASON_NOT_INSTALLED);
    assert_eq!(h.denoise_applied(), Some(false));
    h.tick(600);
    assert_eq!(h.snap().error, "");
    // Idle again, a reload: the cached value, no GPU call.
    h.consumers(0, 1000);
    h.tick(1010);
    assert_eq!(h.state(), State::Idle);
    h.reload("[effect]\ndenoise = true\n", 1100).unwrap();
    assert_eq!(h.world().feature_probes.last(), Some(&Some((8, 6))));
    // The next session does not ask again.
    h.activate(2000);
    let w = h.world();
    assert_eq!((w.cc_reads, w.cc_reads_idle), (1, 0));
}

#[test]
fn a_change_of_what_is_installed_never_requests_an_effect_the_session_started_without() {
    let mut h = H::new(
        "[effect]\nmode = \"blur\"\non_failure = \"none\"\ndenoise = true\n",
        |w| {
            w.effect_unavailable = Some("libVideoFX.so not found".into());
            w.features = Some(all_installed());
            // Learnt once the session is up: this GPU's denoising models
            // are missing.
            w.features_with_cc = Some(denoise_missing("no Denoise model for this GPU (sm_86)"));
            w.compute_capability = Some((8, 6));
        },
    );
    h.activate(0);
    assert_eq!(h.world().feature_probes.last(), Some(&Some((8, 6))));
    assert_eq!(h.applied(), Some(EffectMode::None), "still without it");
    assert_eq!(
        h.denoise_applied(),
        Some(false),
        "only noise removal followed"
    );
    assert!(
        h.world()
            .effect_sets
            .iter()
            .all(|e| e.mode == EffectMode::None),
        "the rejected effect is never requested: {:?}",
        h.world().effect_sets
    );
    let snap = h.snap();
    assert_eq!(
        snap.warning,
        format!(
            "{}; noise removal unavailable: NVIDIA Maxine video effects: {REASON_NOT_INSTALLED}",
            shown_without("libVideoFX.so not found")
        )
    );
    assert_eq!(snap.error, "");
    assert_eq!(h.snap().effect, "blur", "the selection is kept");
    h.tick(1000);
    assert_eq!(h.applied(), Some(EffectMode::None));
    assert_eq!(h.state(), State::Active);
}

#[test]
fn a_session_start_that_learns_what_is_installed_sends_one_settings_changed() {
    let mut h = H::new("[effect]\nmode = \"blur\"\ndenoise = true\n", |w| {
        w.features = Some(all_installed());
        w.features_with_cc = Some(denoise_missing("no Denoise model for this GPU (sm_86)"));
    });
    h.activate(0);
    // The output fails: the camera stops, a fault.
    h.world().failure = Some(Failure::Output("output pipeline failed: boom".into()));
    h.tick(1000);
    assert_eq!(h.state(), State::Error);
    // The next session learns the GPU, and with it that noise removal is
    // not installed for it.
    h.world().compute_capability = Some((8, 6));
    h.signals();
    // One operation: the write, the session it restarts and what that
    // session learns.
    h.set_setting(BLUR_ID, SettingValue::Int(30), 2000).unwrap();
    assert_eq!(h.world().session_starts, 2);
    assert_eq!(h.setting(DENOISE_ID).disabled_reason, REASON_NOT_INSTALLED);
    let sig = h.signals();
    assert_eq!(sig.len(), 1, "one SettingsChanged per operation: {sig:?}");
    let ids = &signal_ids(&sig)[0];
    assert!(
        ids.iter().any(|i| i == BLUR_ID) && ids.iter().any(|i| i == DENOISE_ID),
        "{ids:?}"
    );
    assert_eq!(h.snap().settings_revision, Some(sig[0].0));
}

#[test]
fn the_compute_capability_is_cached_across_restarts_and_refreshed_by_a_session() {
    let mut h = H::new("[effect]\ndenoise = true\n", |w| {
        w.features = Some(all_installed());
        w.compute_capability = Some((8, 6));
    });
    let cache = h.paths.cache.clone().unwrap();
    assert_eq!(h.world().feature_probes, [None], "nothing cached yet");
    h.activate(0);
    assert_eq!(
        fs::read_to_string(crate::gpu_cache::path(&cache)).unwrap(),
        "8.6\n",
        "learnt and kept"
    );
    // A restart probes this GPU's models from the start, with no GPU call.
    h.restart();
    {
        let w = h.world();
        assert_eq!(w.feature_probes, [Some((8, 6))]);
        assert_eq!(w.cc_reads, 0);
    }
    // A garbled or missing file is unknown, never an error.
    for content in [Some("not a version\n"), None] {
        let file = crate::gpu_cache::path(&cache);
        match content {
            Some(c) => fs::write(&file, c).unwrap(),
            None => fs::remove_file(&file).unwrap(),
        }
        h.restart();
        assert_eq!(h.world().feature_probes, [None], "{content:?}");
        assert_eq!(h.snap().error, "");
    }
    // Another GPU since: the session learns it, the cache follows, and the
    // features are read again for it.
    fs::write(crate::gpu_cache::path(&cache), "12.0\n").unwrap();
    h.restart();
    assert_eq!(h.world().feature_probes, [Some((12, 0))]);
    {
        let mut w = h.world();
        w.features = Some(all_installed());
        w.features_with_cc = Some(denoise_missing("no Denoise model for this GPU (sm_86)"));
        w.compute_capability = Some((8, 6));
    }
    h.activate(10_000);
    assert_eq!(h.world().cc_reads, 1);
    assert_eq!(h.world().feature_probes.last(), Some(&Some((8, 6))));
    assert_eq!(
        fs::read_to_string(crate::gpu_cache::path(&cache)).unwrap(),
        "8.6\n"
    );
    assert_eq!(h.setting(DENOISE_ID).disabled_reason, REASON_NOT_INSTALLED);
    // A cache that cannot be written is only logged.
    fs::remove_file(crate::gpu_cache::path(&cache)).unwrap();
    fs::remove_dir(&cache).unwrap();
    fs::write(&cache, b"not a dir").unwrap();
    h.restart();
    h.world().compute_capability = Some((8, 6));
    h.world().features = Some(all_installed());
    h.activate(20_000);
    assert_eq!(h.snap().error, "");
}

#[test]
fn a_global_write_the_state_file_cannot_take_fails_as_the_setter_does() {
    let mut h = H::new("", |_| {});
    let parent = h.paths.state.parent().unwrap().to_path_buf();
    fs::write(&parent, b"not a dir").unwrap();
    let old = h.set_effect("blur", 0).unwrap_err();
    assert!(matches!(&old, IpcError::Failed(m) if m.contains("will not survive a restart")));
    let new = h.set_setting(EFFECT_ID, s("none"), 10).unwrap_err();
    assert_eq!(format!("{new:?}"), format!("{old:?}"));
    assert_eq!(h.snap().effect, "none", "in force all the same");
    h.set_effect("blur", 20).unwrap_err();
    let old = h.set_strength(0.2, 30).unwrap_err();
    let new = h
        .set_setting(BLUR_ID, SettingValue::Int(30), 40)
        .unwrap_err();
    assert_eq!(format!("{new:?}"), format!("{old:?}"));
    assert_eq!(h.snap().blur_strength, 0.3);
    let old = h.set_denoise(true, 50).unwrap_err();
    let new = h
        .set_setting(DENOISE_ID, SettingValue::Bool(false), 60)
        .unwrap_err();
    assert_eq!(format!("{new:?}"), format!("{old:?}"));
    assert!(!h.snap().denoise);
}

// ---- enhancement of a low-resolution camera (the ingest's enhance stage) ----

use crate::settings::effects::REASON_FULL_RESOLUTION;
use obc_core::modules::legacy::ENHANCE as ENHANCE_ID;
use obc_ipc::SettingKind;

/// `c` capturing at 640x360: below the default 1920x1080 output.
fn small(mut c: Camera) -> Camera {
    c.mode = "NV12:640x360@30".parse().unwrap();
    c
}

const NO_VSR: &str = "NvVFX_Load of video super resolution failed (-2)";

fn enhance_error(reason: &str) -> String {
    format!("enhance unavailable: {reason}; the video continues without it")
}

/// Every stage installed but enhancement (video super resolution), missing
/// for `why`.
fn vsr_missing(why: &str) -> Vec<(StageId, Availability)> {
    stage_missing(&StageId::ENHANCE, why)
}

impl H {
    /// Whether each ingest was built with the enhance stage, in order.
    fn enhance_requests(&self) -> Vec<bool> {
        self.world().ingest_enhance.clone()
    }
}

/// The states published since the last drain, and the drained changes.
fn states_in(changes: &[Change]) -> Vec<State> {
    changes
        .iter()
        .filter_map(|c| match c {
            Change::State(s) => Some(*s),
            _ => None,
        })
        .collect()
}

#[test]
fn enhance_toggled_while_streaming_reopens_the_camera_once_and_keeps_active() {
    let mut h = H::new("", |w| w.cameras = vec![small(alpha()), beta()]);
    h.activate(0);
    assert_eq!(h.enhance_requests(), [false]);
    h.signals();
    h.set_setting(ENHANCE_ID, SettingValue::Bool(true), 1000)
        .unwrap();
    // One reopen of the same camera, through the switch (the output holds
    // the last frame): no new session, no state change.
    assert_eq!(h.world().ingests, ["usb-a", "usb-a"]);
    assert_eq!(h.enhance_requests(), [false, true]);
    assert_eq!(h.world().session_starts, 1);
    assert_eq!(saved_now(&h).enhance, Some(true));
    let (changes, _) = h.drain();
    assert_eq!(states_in(&changes), [], "{changes:?}");
    let sig = std::mem::take(&mut h.settings_notes);
    assert_eq!(sig.len(), 1, "one SettingsChanged per operation: {sig:?}");
    assert_eq!(
        sig[0].2,
        [change(ENHANCE_ID, Some(SettingValue::Bool(true)), true, "")]
    );
    h.tick(1100);
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.snap().error, "");
    // The same value again: nothing reopened.
    h.set_setting(ENHANCE_ID, SettingValue::Bool(true), 1200)
        .unwrap();
    assert_eq!(h.enhance_requests(), [false, true]);
    // Off: reopened once more, without it.
    h.set_setting(ENHANCE_ID, SettingValue::Bool(false), 1300)
        .unwrap();
    assert_eq!(h.enhance_requests(), [false, true, false]);
    assert_eq!(h.state(), State::Active);
    // An effect method that does not touch it reopens nothing.
    h.set_strength(0.3, 1400).unwrap();
    assert_eq!(h.enhance_requests(), [false, true, false]);
    // A reset of the video group puts the configuration's value back
    // (off): nothing to reopen.
    h.reset_settings("group:video", 1500).unwrap();
    assert_eq!(saved_now(&h).enhance, None);
    assert_eq!(h.enhance_requests(), [false, true, false]);
}

#[test]
fn enhance_set_while_the_camera_is_closed_applies_at_the_next_open() {
    let mut h = H::new("", |w| w.cameras = vec![small(alpha())]);
    h.set_setting(ENHANCE_ID, SettingValue::Bool(true), 0)
        .unwrap();
    {
        let w = h.world();
        assert!(w.ingests.is_empty());
        assert_eq!(w.session_starts, 0);
        // Nothing of the GPU at idle.
        assert!(w.gpu_log.is_empty(), "{:?}", w.gpu_log);
        assert_eq!((w.cc_reads, w.cc_reads_idle), (0, 0));
    }
    assert_eq!(h.persisted().enhance, Some(true));
    h.activate(100);
    assert_eq!(h.enhance_requests(), [true]);
    // The configuration's key turns it on as well, and survives a restart
    // with the state file's choice over it.
    let mut h = H::new("[effect]\nenhance = true\n", |w| {
        w.cameras = vec![small(alpha())]
    });
    assert_eq!(
        h.setting(ENHANCE_ID).kind,
        SettingKind::Bool {
            default: true,
            value: true
        }
    );
    h.set_setting(ENHANCE_ID, SettingValue::Bool(false), 0)
        .unwrap();
    h.restart();
    h.world().cameras = vec![small(alpha())];
    h.rescan(10);
    h.activate(100);
    assert_eq!(h.enhance_requests(), [false]);
}

#[test]
fn enhance_not_installed_is_never_requested_and_raises_no_error() {
    let why = "no VideoSuperRes model for this GPU in /opt/fake/maxine/lib/models";
    let mut h = H::new("[effect]\nenhance = true\n", |w| {
        w.cameras = vec![small(alpha())];
        w.features = Some(vsr_missing(why));
        // Were it requested, it would fail and show in Error.
        w.enhance_fail = Some(NO_VSR.into());
    });
    let e = h.setting(ENHANCE_ID);
    assert!(!e.enabled);
    assert_eq!(e.disabled_reason, REASON_NOT_INSTALLED);
    assert_eq!(e.kind.value(), Some(SettingValue::Bool(false)), "what runs");
    h.activate(0);
    h.tick(500);
    assert_eq!(h.enhance_requests(), [false]);
    assert_eq!(h.snap().error, "");
    assert!(matches!(
        h.set_setting(ENHANCE_ID, SettingValue::Bool(true), 600),
        Err(IpcError::Disabled(m)) if m == REASON_NOT_INSTALLED
    ));
    // Installed (found on a reload): the camera is reopened with it, and a
    // real failure is reported.
    h.world().features = Some(all_installed());
    h.signals();
    h.reload("[effect]\nenhance = true\n", 1000).unwrap();
    assert_eq!(h.enhance_requests(), [false, true]);
    assert_eq!(
        h.signals()[0].2,
        [change(
            ENHANCE_ID,
            Some(SettingValue::Bool(true)),
            false,
            ""
        )]
    );
    h.tick(1250);
    assert_eq!(h.snap().error, enhance_error(NO_VSR));
    assert_eq!(h.state(), State::Active);
}

#[test]
fn enhance_is_not_requested_for_a_camera_at_full_resolution_and_follows_a_switch() {
    // Alpha captures at the output's size, beta below it.
    let mut h = H::new("[effect]\nenhance = true\n", |w| {
        w.cameras = vec![alpha(), small(beta())]
    });
    let e = h.setting(ENHANCE_ID);
    assert!(!e.enabled);
    assert_eq!(e.disabled_reason, REASON_FULL_RESOLUTION);
    assert_eq!(e.kind.value(), Some(SettingValue::Bool(false)));
    assert!(matches!(
        h.set_setting(ENHANCE_ID, SettingValue::Bool(false), 0),
        Err(IpcError::Disabled(m)) if m == REASON_FULL_RESOLUTION
    ));
    h.activate(0);
    assert_eq!(h.enhance_requests(), [false]);
    // A switch to the small camera: requested, and the setting enabled
    // (in the switch's one signal, structural: another camera).
    h.signals();
    h.call(|r| Command::SelectCamera("usb-b".into(), r), 1000)
        .unwrap();
    assert_eq!(h.enhance_requests(), [false, true]);
    assert_eq!(h.signals().len(), 1);
    let e = h.setting(ENHANCE_ID);
    assert!(e.enabled && e.disabled_reason.is_empty());
    assert_eq!(e.kind.value(), Some(SettingValue::Bool(true)));
    assert_eq!(h.state(), State::Active);
    // Closed, the selection back on alpha: greyed again.
    h.consumers(0, 2000);
    h.tick(10_000);
    assert_eq!(h.state(), State::Idle);
    h.signals();
    h.call(|r| Command::SelectCamera("usb-a".into(), r), 10_100)
        .unwrap();
    assert_eq!(h.signals().len(), 1);
    let e = h.setting(ENHANCE_ID);
    assert!(!e.enabled);
    assert_eq!(e.disabled_reason, REASON_FULL_RESOLUTION);
    assert_eq!(e.kind.value(), Some(SettingValue::Bool(false)));
    // No camera: enabled, it applies when one is used.
    h.world().cameras.clear();
    h.rescan(10_200);
    let e = h.setting(ENHANCE_ID);
    assert!(e.enabled && e.disabled_reason.is_empty());
    assert_eq!(e.kind.value(), Some(SettingValue::Bool(true)));
}

#[test]
fn an_enhance_failure_keeps_streaming_and_shows_in_error_after_noise_removal() {
    let config = "[effect]\nenhance = true\ndenoise = true\non_failure = \"black\"\n";
    let mut h = H::new(config, |w| {
        w.cameras = vec![small(alpha())];
        w.enhance_fail = Some(NO_VSR.into());
        w.denoise_fail = Some(NO_DENOISE.into());
    });
    h.activate(0);
    h.tick(500);
    assert_eq!(h.state(), State::Active, "never a fault");
    assert!(h.world().session.is_some());
    // Noise removal's failure comes first.
    assert_eq!(h.snap().error, denoise_error(NO_DENOISE));
    // Noise removal recovers: the enhance failure shows.
    h.world().denoise_status = EffectStatus::Ready;
    h.drain();
    h.tick(750);
    assert_eq!(h.snap().error, enhance_error(NO_VSR));
    // Reported once, not at every poll; no state moved.
    h.tick(1000);
    h.tick(1250);
    let (changes, _) = h.drain();
    let errors: Vec<_> = changes
        .iter()
        .filter(|c| matches!(c, Change::Error(_)))
        .collect();
    assert_eq!(errors.len(), 1, "{changes:?}");
    assert_eq!(states_in(&changes), []);
    // A new noise removal failure takes over, and gives the place back.
    h.world().denoise_status = EffectStatus::Failed(NO_DENOISE.into());
    h.tick(1500);
    h.tick(1750);
    assert_eq!(h.snap().error, denoise_error(NO_DENOISE));
    h.world().denoise_status = EffectStatus::Ready;
    h.tick(2000);
    assert_eq!(h.snap().error, enhance_error(NO_VSR));
    // A reason with control characters and of any length is made safe and
    // bounded.
    let ugly = format!("bad\u{1b}[31m\n{}", "x".repeat(2000));
    let text = enhance_failure_text(&ugly);
    assert!(!text.chars().any(char::is_control), "{text:?}");
    assert!(text.len() <= STAGE_REASON_MAX + 80, "{}", text.len());
}

#[test]
fn an_enhance_failure_is_cleared_by_the_setting_a_reload_a_switch_and_the_end() {
    let config = "[effect]\nenhance = true\n\
                  [lifecycle]\nidle_grace_s = 0.0\nstart_debounce_ms = 0\n";
    let mut h = H::new(config, |w| {
        w.cameras = vec![small(alpha()), small(beta())];
        w.enhance_fail = Some(NO_VSR.into());
    });
    h.activate(0);
    h.tick(500);
    assert_eq!(h.snap().error, enhance_error(NO_VSR));
    // A write of the setting retries it (a reopen); fixed meanwhile, the
    // error is gone.
    h.world().enhance_fail = None;
    h.set_setting(ENHANCE_ID, SettingValue::Bool(true), 1000)
        .unwrap();
    assert_eq!(h.enhance_requests(), [true, true]);
    h.tick(1250);
    assert_eq!(h.snap().error, "");
    // Switched off: reopened without it.
    h.set_setting(ENHANCE_ID, SettingValue::Bool(false), 1500)
        .unwrap();
    assert_eq!(h.enhance_requests(), [true, true, false]);
    // Failing again, then switched off: cleared.
    h.world().enhance_fail = Some(NO_VSR.into());
    h.set_setting(ENHANCE_ID, SettingValue::Bool(true), 1600)
        .unwrap();
    h.tick(1750);
    assert_eq!(h.snap().error, enhance_error(NO_VSR));
    h.set_setting(ENHANCE_ID, SettingValue::Bool(false), 2000)
        .unwrap();
    assert_eq!(h.enhance_requests().last(), Some(&false));
    assert_eq!(h.snap().error, "");
    h.tick(2250);
    assert_eq!(h.snap().error, "");
    // A reload retries it.
    h.set_setting(ENHANCE_ID, SettingValue::Bool(true), 2500)
        .unwrap();
    h.tick(2750);
    assert_eq!(h.snap().error, enhance_error(NO_VSR));
    let n = h.enhance_requests().len();
    h.world().enhance_fail = None;
    h.reload(config, 3000).unwrap();
    assert_eq!(h.enhance_requests().len(), n + 1, "retried");
    h.tick(3250);
    assert_eq!(h.snap().error, "");
    // A camera switch: a new ingest, its own status.
    h.world().enhance_fail = Some(NO_VSR.into());
    h.set_setting(ENHANCE_ID, SettingValue::Bool(false), 3400)
        .unwrap();
    h.set_setting(ENHANCE_ID, SettingValue::Bool(true), 3500)
        .unwrap();
    h.tick(3750);
    assert_eq!(h.snap().error, enhance_error(NO_VSR));
    h.world().enhance_fail = None;
    h.call(|r| Command::SelectCamera("usb-b".into(), r), 4000)
        .unwrap();
    h.tick(4250);
    assert_eq!(h.snap().error, "");
    // The end of the session.
    h.world().enhance_fail = Some(NO_VSR.into());
    h.call(|r| Command::SelectCamera("usb-a".into(), r), 4500)
        .unwrap();
    h.tick(4750);
    assert_eq!(h.snap().error, enhance_error(NO_VSR));
    h.consumers(0, 5000);
    h.tick(5100);
    assert_eq!(h.state(), State::Idle);
    assert_eq!(h.snap().error, "");
    assert_eq!(h.ctrl.enhance_failure, None);
}

/// A camera whose frames the stage cannot read once uploaded (NV21 goes to
/// the GPU as it comes, and `obcenhance` reads NV12 and I420): greyed with
/// its own reason, and its ingest built without the stage.
#[test]
fn enhance_is_not_requested_for_a_camera_in_a_format_it_cannot_read() {
    use crate::settings::effects::REASON_FORMAT;
    let mut nv21 = alpha();
    nv21.mode = "NV21:640x360@30".parse().unwrap();
    let mut h = H::new("[effect]\nenhance = true\n", |w| {
        w.cameras = vec![nv21, small(beta())]
    });
    let e = h.setting(ENHANCE_ID);
    assert!(!e.enabled);
    assert_eq!(e.disabled_reason, REASON_FORMAT);
    assert!(REASON_FORMAT.len() <= 96);
    assert_eq!(e.kind.value(), Some(SettingValue::Bool(false)));
    assert!(matches!(
        h.set_setting(ENHANCE_ID, SettingValue::Bool(true), 0),
        Err(IpcError::Disabled(m)) if m == REASON_FORMAT
    ));
    h.activate(0);
    assert_eq!(h.enhance_requests(), [false]);
    assert_eq!(h.state(), State::Active);
    // The NV12 camera: requested.
    h.call(|r| Command::SelectCamera("usb-b".into(), r), 1000)
        .unwrap();
    assert_eq!(h.enhance_requests(), [false, true]);
    // An MJPEG camera is decoded to I420 on the CPU: it applies.
    let mut h = H::new("[effect]\nenhance = true\n", |w| {
        let mut c = alpha();
        c.mode = "MJPEG:640x360@30".parse().unwrap();
        w.cameras = vec![c]
    });
    assert!(h.setting(ENHANCE_ID).enabled);
    h.activate(0);
    assert_eq!(h.enhance_requests(), [true]);
}

/// 1280x720 into the 1920x1080 output still pays (under half the pixels);
/// 1600x900 does not.
#[test]
fn enhance_applies_only_where_it_pays() {
    for (mode, applies) in [("NV12:1280x720@30", true), ("NV12:1600x900@30", false)] {
        let mut h = H::new("[effect]\nenhance = true\n", |w| {
            let mut c = alpha();
            c.mode = mode.parse().unwrap();
            w.cameras = vec![c]
        });
        let e = h.setting(ENHANCE_ID);
        assert_eq!(e.enabled, applies, "{mode}");
        if !applies {
            assert_eq!(e.disabled_reason, REASON_FULL_RESOLUTION, "{mode}");
        }
    }
}

/// A noise removal or enhancement failure shows in `Error` only when no
/// higher cause holds (the writer missing, a fault, noise removal over
/// enhancement), and over a damaged state file. An effect failure is no
/// such cause: it is in `Warning`.
#[test]
fn stage_failures_never_overwrite_a_higher_cause_in_error() {
    let failed = |r: &str| EffectStatus::Failed(r.into());
    let fresh = || {
        let mut h = H::new("", |w| w.cameras = vec![small(alpha())]);
        h.ctrl.error.clear();
        h
    };
    type Raise = fn(&mut Controller<FakeBackend>);
    let higher: [(&str, Raise); 2] = [
        ("writer", |c| {
            c.writer = None;
            c.writer_error = Some("no loopback device".into());
        }),
        ("fault", |c| c.fault = Some("the output failed".into())),
    ];
    for (what, raise) in higher {
        let mut h = fresh();
        raise(&mut h.ctrl);
        let top = h.ctrl.standing_error();
        assert!(!top.is_empty(), "{what}");
        h.ctrl.error = top.clone();
        h.ctrl
            .poll_denoise(failed("no denoiser"), &EffectStatus::Off);
        assert_eq!(h.ctrl.error, top, "noise removal under {what}");
        h.ctrl.poll_enhance(failed("no vsr"));
        assert_eq!(h.ctrl.error, top, "enhance under {what}");
        assert!(h.ctrl.denoise_failure().is_some() && h.ctrl.enhance_failure.is_some());
    }
    let mut h = fresh();
    h.ctrl.effect_failure = Some("effect unavailable: x".into());
    assert_eq!(
        h.ctrl.standing_error(),
        "",
        "an effect failure is not in Error"
    );
    h.ctrl
        .poll_denoise(failed("no denoiser"), &EffectStatus::Off);
    assert_eq!(h.ctrl.error, denoise_failure_text("no denoiser"));
    // Noise removal over enhancement, either order.
    let mut h = fresh();
    h.ctrl
        .poll_denoise(failed("no denoiser"), &EffectStatus::Off);
    h.ctrl.poll_enhance(failed("no vsr"));
    assert_eq!(h.ctrl.error, denoise_failure_text("no denoiser"));
    let mut h = fresh();
    h.ctrl.poll_enhance(failed("no vsr"));
    assert_eq!(h.ctrl.error, enhance_error("no vsr"));
    h.ctrl
        .poll_denoise(failed("no denoiser"), &EffectStatus::Off);
    assert_eq!(h.ctrl.error, denoise_failure_text("no denoiser"));
    // Over a damaged state file, which is lower.
    let mut h = fresh();
    h.ctrl.state_damaged = Some("state.toml is damaged".into());
    h.ctrl.error = "state.toml is damaged".into();
    h.ctrl.poll_enhance(failed("no vsr"));
    assert_eq!(h.ctrl.error, enhance_error("no vsr"));
}

/// Toggling enhancement before the camera delivers reopens it each time,
/// but the start timeout runs from the first open: the camera still fails
/// over at 10 s.
#[test]
fn reopens_before_the_first_frame_keep_the_start_deadline() {
    let mut h = H::new("", |w| {
        w.frames = false;
        w.cameras = vec![small(alpha()), small(beta())];
    });
    h.consumers(1, 0);
    h.tick(300);
    assert_eq!(h.state(), State::Starting);
    assert_eq!(h.world().ingests, ["usb-a"]);
    // A toggle every 3 s, each a reopen of alpha.
    let mut on = true;
    for t in [3_000, 6_000, 9_000] {
        h.set_setting(ENHANCE_ID, SettingValue::Bool(on), t)
            .unwrap();
        on = !on;
        h.tick(t + 100);
    }
    assert_eq!(h.world().ingests, ["usb-a"; 4], "three reopens");
    assert_eq!(h.enhance_requests(), [false, true, false, true]);
    h.tick(10_299);
    assert_eq!(h.world().ingests.len(), 4, "not yet");
    h.tick(10_300);
    assert_eq!(
        h.world().ingests.last().map(String::as_str),
        Some("usb-b"),
        "alpha failed over at its first deadline"
    );
}

// ---- relighting (the `lighting` group) ----

use obc_core::RelightLight;
use obc_core::modules::legacy::{
    BRIGHTNESS as BRIGHTNESS_ID, DIRECTION as DIRECTION_ID, LIGHT as LIGHT_ID,
    RELIGHT as RELIGHT_ID,
};

/// The Maxine module's reason of a relighting setting while relighting is
/// off.
const REASON_TURN_ON_RELIGHT: &str = "Turn on relighting to change this";

const NO_RELIGHT: &str = "NvVFX_Load of relighting failed (-2)";

fn relight_error(reason: &str) -> String {
    format!("relighting unavailable: {reason}; the video continues without it")
}

/// Every stage installed but relighting, whose feature `missing` (its own,
/// or the green screen that makes its matte) is missing for `why`, as the
/// Maxine backend reports it (`<feature>: <why>`); the green screen takes
/// the background stage with it.
fn feature_missing(missing: &str, why: &str) -> Vec<(StageId, Availability)> {
    let mut stages = stage_missing(&StageId::RELIGHT, &format!("{missing}: {why}"));
    if missing == "green-screen" {
        stages[0].1 = Availability::NotInstalled(format!("{missing}: {why}"));
    }
    stages
}

impl H {
    /// What the running session was given of relighting: on, the light,
    /// the direction and the brightness.
    fn relight_applied(&self) -> Option<(bool, RelightLight, i32, u32)> {
        self.world().effect.as_ref().map(|e| {
            (
                e.relight(),
                e.relight_light(),
                e.relight_direction(),
                e.relight_brightness(),
            )
        })
    }
}

#[test]
fn each_relighting_setting_is_saved_applied_live_and_announced_once() {
    let mut h = H::new("", |w| w.features = Some(all_installed()));
    h.activate(0);
    h.signals();
    assert_eq!(
        h.relight_applied(),
        Some((false, RelightLight::Neutral, 0, 100))
    );
    // The dependents are greyed until relighting is on.
    let d = h.setting(DIRECTION_ID);
    assert!(!d.enabled);
    assert_eq!(d.disabled_reason, REASON_TURN_ON_RELIGHT);
    assert!(matches!(
        h.set_setting(DIRECTION_ID, SettingValue::Int(-30), 500),
        Err(IpcError::Disabled(m)) if m == REASON_TURN_ON_RELIGHT
    ));
    let sets = h.world().effect_sets.len();
    // On: saved, applied, and one signal with the three it enables.
    h.set_setting(RELIGHT_ID, SettingValue::Bool(true), 1000)
        .unwrap();
    assert_eq!(saved_now(&h).relight, Some(true));
    assert_eq!(
        h.relight_applied(),
        Some((true, RelightLight::Neutral, 0, 100))
    );
    let sig = h.signals();
    assert_eq!(
        sig[0].2,
        [
            change(RELIGHT_ID, Some(SettingValue::Bool(true)), true, ""),
            change(LIGHT_ID, Some(s("neutral")), false, ""),
            change(DIRECTION_ID, Some(SettingValue::Int(0)), false, ""),
            change(BRIGHTNESS_ID, Some(SettingValue::Int(100)), false, ""),
        ]
    );
    assert_eq!(sig.len(), 1);
    h.tick(1100);
    assert_eq!(h.world().relight_status, EffectStatus::Ready);
    // The light, the direction (negative) and the brightness: each one
    // set_effect, one signal, saved at once.
    h.set_setting(LIGHT_ID, s("warm"), 1200).unwrap();
    assert_eq!(saved_now(&h).relight_light, Some(RelightLight::Warm));
    assert_eq!(signal_ids(&h.signals()), [vec![LIGHT_ID]]);
    h.set_setting(DIRECTION_ID, SettingValue::Int(-30), 1300)
        .unwrap();
    assert_eq!(saved_now(&h).relight_direction, Some(-30));
    assert_eq!(signal_ids(&h.signals()), [vec![DIRECTION_ID]]);
    h.set_setting(BRIGHTNESS_ID, SettingValue::Int(150), 1400)
        .unwrap();
    assert_eq!(saved_now(&h).relight_brightness, Some(150));
    assert_eq!(
        h.signals()[0].2,
        [change(
            BRIGHTNESS_ID,
            Some(SettingValue::Int(150)),
            true,
            ""
        )]
    );
    assert_eq!(
        h.relight_applied(),
        Some((true, RelightLight::Warm, -30, 150))
    );
    assert_eq!(h.world().effect_sets.len(), sets + 4);
    // A slider's stream: every value answered at once, no coalescing.
    for (i, v) in [-25, -20, -15].into_iter().enumerate() {
        h.set_setting(DIRECTION_ID, SettingValue::Int(v), 1500 + i as u64)
            .unwrap();
    }
    assert_eq!(h.relight_applied().unwrap().2, -15);
    assert_eq!(h.world().effect_sets.len(), sets + 7);
    assert_eq!(h.signals().len(), 3, "one per value");
    // A refused value changes nothing.
    assert!(matches!(
        h.set_setting(DIRECTION_ID, SettingValue::Int(-32), 1600),
        Err(IpcError::InvalidArgument(_))
    ));
    assert!(matches!(
        h.set_setting(BRIGHTNESS_ID, SettingValue::Int(250), 1600),
        Err(IpcError::InvalidArgument(_))
    ));
    assert_eq!(h.relight_applied().unwrap().2, -15);
    assert_eq!(h.state(), State::Active, "never restarted");
    assert_eq!(h.world().session_starts, 1);
    assert_eq!(h.snap().error, "");
    // Off: the three greyed again, in one signal.
    h.set_setting(RELIGHT_ID, SettingValue::Bool(false), 1700)
        .unwrap();
    assert!(!h.relight_applied().unwrap().0);
    assert_eq!(
        signal_ids(&h.signals()),
        [vec![RELIGHT_ID, LIGHT_ID, DIRECTION_ID, BRIGHTNESS_ID]]
    );
    // Survives a restart.
    h.restart();
    assert_eq!(
        h.setting(DIRECTION_ID).kind.value(),
        Some(SettingValue::Int(-15))
    );
    let st = saved_now(&h);
    assert_eq!(
        (
            st.relight,
            st.relight_light,
            st.relight_direction,
            st.relight_brightness
        ),
        (Some(false), Some(RelightLight::Warm), Some(-15), Some(150))
    );
}

/// A new light and a stage toggled on or off are each one change of the
/// running session's effect, on the session as it runs: one `set_effect`
/// that differs from the previous one in that setting alone (so the element
/// switches that stage alone: `gst-obc`'s
/// `gpu_a_stage_change_loads_or_releases_that_stage_alone`), no restart, no
/// camera reopen, and the other stage left as it is.
#[test]
fn a_new_light_or_a_stage_toggle_is_one_change_on_the_running_session() {
    let mut h = H::new("[effect]\ndenoise = true\nrelight = true\n", |w| {
        w.features = Some(all_installed())
    });
    h.activate(0);
    h.tick(100);
    assert_eq!(h.world().denoise_status, EffectStatus::Ready);
    assert_eq!(h.world().relight_status, EffectStatus::Ready);
    let ingests = h.world().ingests.clone();
    let mut t = 200;
    type Change = fn(&mut EffectRun);
    let steps: [(&str, SettingValue, Change); 7] = [
        (LIGHT_ID, s("warm"), |e| {
            e.set_param("relight", "light", "warm")
        }),
        (LIGHT_ID, s("cool"), |e| {
            e.set_param("relight", "light", "cool")
        }),
        (DENOISE_ID, SettingValue::Bool(false), |e| {
            e.set_enabled("denoise", false)
        }),
        (DENOISE_ID, SettingValue::Bool(true), |e| {
            e.set_enabled("denoise", true)
        }),
        (RELIGHT_ID, SettingValue::Bool(false), |e| {
            e.set_enabled("relight", false)
        }),
        (RELIGHT_ID, SettingValue::Bool(true), |e| {
            e.set_enabled("relight", true)
        }),
        (LIGHT_ID, s("neutral"), |e| {
            e.set_param("relight", "light", "neutral")
        }),
    ];
    for (id, value, change) in steps {
        let label = format!("{id} = {value:?}");
        let sets = h.world().effect_sets.len();
        let mut want = h.world().effect.clone().unwrap();
        change(&mut want);
        h.set_setting(id, value, t).unwrap();
        {
            let w = h.world();
            assert_eq!(w.effect_sets.len(), sets + 1, "{label}: one change");
            assert_eq!(
                w.effect_sets.last(),
                Some(&want),
                "{label}: that setting alone"
            );
        }
        h.tick(t + 50);
        let w = h.world();
        assert_eq!(w.session_starts, 1, "{label}: never restarted");
        assert_eq!(w.ingests, ingests, "{label}: the camera not reopened");
        assert_eq!(
            w.effect_sets.len(),
            sets + 1,
            "{label}: nothing given again"
        );
        drop(w);
        assert_eq!(h.state(), State::Active, "{label}");
        t += 100;
    }
    assert_eq!(h.snap().error, "");
}

#[test]
fn relighting_settings_at_idle_are_only_stored() {
    let mut h = H::new("", |w| w.features = Some(all_installed()));
    h.signals();
    h.set_setting(RELIGHT_ID, SettingValue::Bool(true), 0)
        .unwrap();
    h.set_setting(LIGHT_ID, s("cool"), 10).unwrap();
    h.set_setting(DIRECTION_ID, SettingValue::Int(45), 20)
        .unwrap();
    h.set_setting(BRIGHTNESS_ID, SettingValue::Int(75), 30)
        .unwrap();
    assert_eq!(h.signals().len(), 4, "one per operation");
    {
        let w = h.world();
        assert!(w.session.is_none() && w.effect_sets.is_empty());
        assert_eq!((w.session_starts, w.cc_reads), (0, 0), "nothing at idle");
    }
    let st = saved_now(&h);
    assert_eq!(
        (
            st.relight,
            st.relight_light,
            st.relight_direction,
            st.relight_brightness
        ),
        (Some(true), Some(RelightLight::Cool), Some(45), Some(75))
    );
    // The next session starts with them.
    h.activate(100);
    assert_eq!(
        h.relight_applied(),
        Some((true, RelightLight::Cool, 45, 75))
    );
}

#[test]
fn relighting_not_installed_is_never_requested_and_raises_no_error() {
    for missing in ["relighting", "green-screen"] {
        let mut h = H::new("[effect]\nrelight = true\n", |w| {
            w.features = Some(feature_missing(missing, "no model for this GPU"));
            // Were it requested, it would fail and show in Error.
            w.relight_fail = Some(NO_RELIGHT.into());
        });
        let r = h.setting(RELIGHT_ID);
        assert!(!r.enabled, "{missing}");
        assert_eq!(r.disabled_reason, REASON_NOT_INSTALLED);
        assert_eq!(r.kind.value(), Some(SettingValue::Bool(false)), "what runs");
        for id in [LIGHT_ID, DIRECTION_ID, BRIGHTNESS_ID] {
            assert_eq!(h.setting(id).disabled_reason, REASON_NOT_INSTALLED, "{id}");
        }
        h.activate(0);
        h.tick(500);
        h.tick(750);
        assert_eq!(h.relight_applied().map(|r| r.0), Some(false), "{missing}");
        assert_eq!(h.world().relight_status, EffectStatus::Off);
        assert_eq!(h.snap().error, "", "{missing}");
        assert!(matches!(
            h.set_setting(RELIGHT_ID, SettingValue::Bool(true), 800),
            Err(IpcError::Disabled(m)) if m == REASON_NOT_INSTALLED
        ));
        // Installed (found on a reload): it turns on by itself, and a real
        // failure is reported.
        h.world().features = Some(all_installed());
        h.signals();
        h.reload("[effect]\nrelight = true\n", 1000).unwrap();
        assert_eq!(
            signal_ids(&h.signals()),
            [vec![RELIGHT_ID, LIGHT_ID, DIRECTION_ID, BRIGHTNESS_ID]]
        );
        assert_eq!(h.relight_applied().map(|r| r.0), Some(true));
        h.tick(1250);
        assert_eq!(h.snap().error, relight_error(NO_RELIGHT));
        assert_eq!(h.state(), State::Active);
    }
}

#[test]
fn a_relighting_failure_keeps_streaming_and_is_cleared_like_noise_removals() {
    let config = "[effect]\nrelight = true\n\
                  [lifecycle]\nidle_grace_s = 0.0\nstart_debounce_ms = 0\n";
    for on_failure in ["black", "none"] {
        let config = config.replace(
            "relight = true\n",
            &format!("relight = true\non_failure = \"{on_failure}\"\n"),
        );
        let mut h = H::new(&config, |w| w.relight_fail = Some(NO_RELIGHT.into()));
        h.activate(0);
        h.tick(500);
        assert_eq!(h.state(), State::Active, "{on_failure}");
        assert_eq!(h.snap().error, relight_error(NO_RELIGHT), "{on_failure}");
        // Ready again: gone.
        h.world().relight_status = EffectStatus::Ready;
        h.tick(1000);
        assert_eq!(h.snap().error, "");
        assert_eq!(h.ctrl.relight_failure(), None);
    }
    // A write of a relighting setting retries it (the value in force too).
    let mut h = H::new(config, |w| w.relight_fail = Some(NO_RELIGHT.into()));
    h.activate(0);
    h.tick(500);
    assert_eq!(h.snap().error, relight_error(NO_RELIGHT));
    let sets = h.world().effect_sets.len();
    h.set_setting(RELIGHT_ID, SettingValue::Bool(true), 600)
        .unwrap();
    assert_eq!(h.world().effect_sets.len(), sets + 1, "retried");
    assert_eq!(h.snap().error, relight_error(NO_RELIGHT), "again");
    // A per-frame parameter is given to the session, never as a retry: the
    // failure and its Error stay (a slider would reload the model per tick).
    let sets = h.world().effect_sets.len();
    for (i, (id, v)) in [(DIRECTION_ID, 10), (BRIGHTNESS_ID, 150), (DIRECTION_ID, 15)]
        .into_iter()
        .enumerate()
    {
        h.set_setting(id, SettingValue::Int(v), 650 + i as u64)
            .unwrap();
    }
    let given = h.world().effect_sets[sets..].to_vec();
    assert_eq!(given.len(), 3, "each value reaches the session");
    assert_eq!(
        given
            .iter()
            .map(|e| (e.relight_direction(), e.relight_brightness()))
            .collect::<Vec<_>>(),
        [(10, 100), (10, 150), (15, 150)]
    );
    assert_eq!(h.snap().error, relight_error(NO_RELIGHT), "not retried");
    assert!(h.ctrl.relight_failure().is_some());
    // A toggle write retries.
    h.world().relight_fail = None;
    h.set_setting(RELIGHT_ID, SettingValue::Bool(true), 700)
        .unwrap();
    assert_eq!(h.world().relight_status, EffectStatus::Ready);
    assert_eq!(h.snap().error, "");
    // A reset of the group clears it as well.
    h.world().relight_status = EffectStatus::Failed(NO_RELIGHT.into());
    h.tick(800);
    assert_eq!(h.snap().error, relight_error(NO_RELIGHT));
    h.reset_settings("group:lighting", 900).unwrap();
    assert_eq!(h.snap().error, "");
    assert_eq!(saved_now(&h).relight_direction, None);
    assert_eq!(h.relight_applied().unwrap().2, 0, "the configuration's");
    // A reload retries it.
    h.world().relight_status = EffectStatus::Failed(NO_RELIGHT.into());
    h.tick(1000);
    assert_eq!(h.snap().error, relight_error(NO_RELIGHT));
    let sets = h.world().effect_sets.len();
    h.reload(config, 1100).unwrap();
    assert_eq!(h.world().effect_sets.len(), sets + 1);
    assert_eq!(h.snap().error, "");
    // The end of the session ends it.
    h.world().relight_status = EffectStatus::Failed(NO_RELIGHT.into());
    h.tick(1200);
    assert_eq!(h.snap().error, relight_error(NO_RELIGHT));
    h.consumers(0, 1300);
    h.tick(1400);
    assert_eq!(h.state(), State::Idle);
    assert_eq!(h.snap().error, "");
    assert_eq!(h.ctrl.relight_failure(), None);
}

#[test]
fn a_relighting_failure_text_is_sanitised_and_bounded() {
    let long = format!("bad\nline\t{}", "x".repeat(2000));
    let text = relight_failure_text(&long);
    assert!(
        text.starts_with("relighting unavailable: bad line "),
        "{text}"
    );
    assert!(text.ends_with("; the video continues without it"));
    assert!(!text.chars().any(char::is_control), "{text}");
    assert!(text.len() <= STAGE_REASON_MAX + 80, "{}", text.len());
}

#[test]
fn a_whole_chain_failure_is_the_effects_not_relightings() {
    let config = "[effect]\nmode = \"blur\"\nrelight = true\non_failure = \"none\"\n";
    let reason = "the green screen did not load";
    let mut h = H::new(config, |w| {
        w.effect_fail = Some(reason.into());
        w.effect_fail_chain = true;
    });
    h.activate(0);
    h.tick(500);
    h.tick(750);
    assert_eq!(
        h.world().relight_status,
        EffectStatus::Failed(reason.into())
    );
    assert_eq!(h.snap().warning, shown_without(reason));
    assert_eq!(h.snap().error, "");
    assert_eq!(h.ctrl.relight_failure(), None);
}

/// Relighting comes after noise removal and before enhancement in
/// `Error`, either order, and under the higher causes.
#[test]
fn a_relighting_failure_sits_between_noise_removal_and_enhancement() {
    let failed = |r: &str| EffectStatus::Failed(r.into());
    let fresh = || {
        let mut h = H::new("", |w| w.cameras = vec![small(alpha())]);
        h.ctrl.error.clear();
        h
    };
    // A fault over relighting.
    let mut h = fresh();
    h.ctrl.fault = Some("the output failed".into());
    h.ctrl.error = "the output failed".into();
    h.ctrl
        .poll_relight(failed("no relight"), &EffectStatus::Off);
    assert_eq!(h.ctrl.error, "the output failed");
    assert!(h.ctrl.relight_failure().is_some());
    // An effect failure is in `Warning`, never over it in `Error`.
    let mut h = fresh();
    h.ctrl.effect_failure = Some("effect unavailable: x".into());
    h.ctrl
        .poll_relight(failed("no relight"), &EffectStatus::Off);
    assert_eq!(h.ctrl.error, relight_error("no relight"));
    // Noise removal over relighting.
    let mut h = fresh();
    h.ctrl
        .poll_relight(failed("no relight"), &EffectStatus::Off);
    assert_eq!(h.ctrl.error, relight_error("no relight"));
    h.ctrl
        .poll_denoise(failed("no denoiser"), &EffectStatus::Off);
    assert_eq!(h.ctrl.error, denoise_error("no denoiser"));
    let mut h = fresh();
    h.ctrl
        .poll_denoise(failed("no denoiser"), &EffectStatus::Off);
    h.ctrl
        .poll_relight(failed("no relight"), &EffectStatus::Off);
    assert_eq!(h.ctrl.error, denoise_error("no denoiser"));
    // Relighting over enhancement.
    let mut h = fresh();
    h.ctrl.poll_enhance(failed("no vsr"));
    h.ctrl
        .poll_relight(failed("no relight"), &EffectStatus::Off);
    assert_eq!(h.ctrl.error, relight_error("no relight"));
    let mut h = fresh();
    h.ctrl
        .poll_relight(failed("no relight"), &EffectStatus::Off);
    h.ctrl.poll_enhance(failed("no vsr"));
    assert_eq!(h.ctrl.error, relight_error("no relight"));
    // It ends: the enhancement failure, still standing, shows.
    h.ctrl.poll_relight(EffectStatus::Ready, &EffectStatus::Off);
    assert_eq!(h.ctrl.error, enhance_error("no vsr"));
    // Over a damaged state file.
    let mut h = fresh();
    h.ctrl.state_damaged = Some("state.toml is damaged".into());
    h.ctrl.error = "state.toml is damaged".into();
    h.ctrl
        .poll_relight(failed("no relight"), &EffectStatus::Off);
    assert_eq!(h.ctrl.error, relight_error("no relight"));
}

#[test]
fn a_damaged_relighting_value_in_the_state_file_fails_closed() {
    let mut h = H::new("", |_| {});
    fs::create_dir_all(h.paths.state.parent().unwrap()).unwrap();
    fs::write(&h.paths.state, "enabled = true\nrelight_direction = 120\n").unwrap();
    h.restart();
    assert_eq!(h.state(), State::Off);
    let error = h.snap().error;
    assert!(
        error.contains("state.toml") && error.contains("relight_direction"),
        "{error}"
    );
}

// ---- the modules (revision 9) ----

use crate::backend::{Backends, Placement};
use crate::modules::{LEDGER_FILE, MAXINE_MODULE};
use obc_module_host::{CrashLedger, REASON_CRASHED};

impl H {
    fn modules(&mut self) -> obc_ipc::ModuleList {
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let now = self.at(0);
        assert!(self.ctrl.handle(Command::GetModules(tx), now));
        rx.try_recv().unwrap().unwrap()
    }

    fn global_groups(&mut self) -> Vec<String> {
        self.get_settings("", 0)
            .unwrap()
            .groups
            .iter()
            .filter(|g| g.is_global())
            .map(|g| g.id.clone())
            .collect()
    }
}

/// The example module, built (once per test binary) out of tree as a
/// third party builds it, with a manifest naming it in a `modules.d`
/// directory of its own; the directory is returned (kept by the caller).
fn hello_modules_dir() -> TempDir {
    static BUILT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    let lib = BUILT.get_or_init(|| {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let target = repo.join("target/daemon-module-tests");
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "--quiet", "--locked", "--manifest-path"])
            .arg(repo.join("examples/module-hello/Cargo.toml"))
            .arg("--target-dir")
            .arg(&target)
            .status()
            .expect("run cargo");
        assert!(status.success(), "building the example module failed");
        target.join("debug/libobc_module_hello.so")
    });
    let dir = tempfile::tempdir().unwrap();
    let manifest = fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/module-hello/hello.toml"),
    )
    .unwrap()
    .replace(
        "library = \"target/release/libobc_module_hello.so\"",
        &format!("library = \"{}\"", lib.display()),
    );
    fs::write(dir.path().join("hello.toml"), manifest).unwrap();
    dir
}

fn modules_config(dir: &std::path::Path) -> String {
    format!(
        "[modules]\ndirs = [\"{}\"]\n[lifecycle]\nstart_debounce_ms = 0\n",
        dir.display()
    )
}

#[test]
fn with_no_module_the_camera_passes_through_and_no_effect_group_is_shown() {
    let mut h = H::new("", |w| w.maxine = false);
    assert!(h.modules().modules.is_empty());
    assert!(h.global_groups().is_empty());
    assert!(matches!(
        h.set_setting(EFFECT_ID, s("blur"), 0),
        Err(IpcError::UnknownSetting(_))
    ));
    h.activate(100);
    {
        let w = h.world();
        let (params, _) = w.session.as_ref().unwrap();
        assert_eq!(params.backends, Backends::default(), "no module code runs");
        assert!(params.effect.stages.is_empty() && params.effect.chains.is_empty());
        assert!(!params.effect.enhance);
    }
    assert_eq!(h.snap().error, "");
    assert!(h.ctrl.order_ids().is_empty());
}

/// `Warning` of a selected `blur` whose module (Maxine) is disabled.
const NO_BLUR: &str = "effect blur unavailable: NVIDIA Maxine video effects: Disabled";

/// `Warning` of a selected `blur` no installed module provides.
const NO_MODULE_BLUR: &str = "effect blur unavailable: No installed module provides it";

#[test]
fn a_background_effect_with_no_module_to_run_it_is_dropped_with_a_warning() {
    let mut h = H::new("[effect]\nmode = \"blur\"\n", |w| w.maxine = false);
    // The method of revision 8 still takes the choice.
    h.call(|r| Command::SetEffect("blur".into(), r), 0).unwrap();
    assert_eq!(
        h.snap().warning,
        NO_MODULE_BLUR,
        "known before a session starts"
    );
    h.consumers(1, 100);
    h.tick(400);
    // The feed streams without it, closed by default or not.
    assert_eq!(h.state(), State::Active);
    let snap = h.snap();
    assert_eq!(
        (snap.error.as_str(), snap.warning.as_str()),
        ("", NO_MODULE_BLUR)
    );
    assert_eq!(snap.effect, "blur", "the choice is kept");
    assert_eq!(h.applied(), Some(EffectMode::None));
    // A call for the effect is taken, and changes nothing of that.
    h.set_effect("blur", 500).unwrap();
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.applied(), Some(EffectMode::None));
    assert_eq!(h.snap().error, "");
    assert_eq!(h.persisted().effect, Some(EffectMode::Blur));
}

#[test]
fn the_maxine_module_offers_the_groups_and_stages_of_revision_8() {
    let mut h = H::new("", |_| {});
    assert_eq!(h.global_groups(), ["background", "video", "lighting"]);
    let list = h.modules();
    assert_eq!(list.serial, 1);
    let m = list.find(MAXINE_MODULE).unwrap();
    assert_eq!((m.enabled, m.status.as_str()), (true, "available"));
    let stages: Vec<(&str, &str, bool)> = m
        .stages
        .iter()
        .map(|s| (s.id.as_str(), s.slot.as_str(), s.selected))
        .collect();
    assert_eq!(
        stages,
        [
            ("enhance", "pre-matte", true),
            ("denoise", "pre-matte", true),
            ("background", "matte", true),
            ("relight", "post-matte", true),
            // The manifest declares it; the fake's probe does not offer it.
            ("frame-generation", "post-composite", false)
        ]
    );
    // It is the effect element's backend and the ingest's enhancer.
    h.activate(0);
    let w = h.world();
    let (params, _) = w.session.as_ref().unwrap();
    assert_eq!(
        params.backends.effect.as_ref().map(|c| c.id.as_str()),
        Some(MAXINE_MODULE)
    );
    assert_eq!(
        params.backends.enhance.as_ref().map(|c| c.id.as_str()),
        Some(MAXINE_MODULE)
    );
    let ids: Vec<&str> = params.effect.stages.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["denoise", "relight"]);
    assert!(params.effect.chains.is_empty());
    // Its configuration: the module's table, the revision-8 SDK root
    // alias, and the GPU's compute capability when known.
    let (_, config) = w
        .module_opens
        .iter()
        .find(|(id, _)| id == MAXINE_MODULE)
        .unwrap();
    assert_eq!(
        config["_host"]["compute_capability"],
        serde_json::Value::Null
    );
}

/// A reload keeps a module open while its manifest, library and
/// configuration are unchanged (probed again), and opens anew only the
/// modules whose configuration (or manifest) changed.
#[test]
fn a_reload_reopens_only_the_modules_that_changed() {
    const NPU: &str = "org.example.npu";
    let manifest = |version: &str| {
        crate::modules::testing::entry(
            &format!(
                "abi = 1\nid = \"{NPU}\"\nname = \"NPU cut-out\"\nversion = \"{version}\"\n\
                 licence = \"MIT\"\nkind = \"video-stage\"\nlibrary = \"/m/libnpu.so\"\n\
                 [[stage]]\nid = \"background\"\nslot = \"matte\"\nrank = 100\n\
                 device = \"npu\"\nframes = [\"host\"]\nformats = [\"nv12\"]\n"
            ),
            "/m/npu.toml",
        )
    };
    let opened = Ok(crate::backend::OpenedModule {
        module: None,
        stages: vec![(
            StageId::BACKGROUND,
            obc_pipeline::effect::Availability::Installed,
        )],
        settings: Default::default(),
    });
    let first = (manifest("1"), opened.clone());
    let mut h = H::new("", move |w| w.modules = vec![first]);
    let opens = |h: &H, id: &str| {
        h.world()
            .module_opens
            .iter()
            .filter(|(m, _)| m == id)
            .count()
    };
    assert_eq!((opens(&h, MAXINE_MODULE), opens(&h, NPU)), (1, 1));
    h.reload("", 0).unwrap();
    assert_eq!(
        (opens(&h, MAXINE_MODULE), opens(&h, NPU)),
        (1, 1),
        "nothing changed: both kept"
    );
    assert_eq!(h.modules().find(NPU).unwrap().status, "available");
    h.reload(&format!("[module.\"{NPU}\"]\nthreads = 2\n"), 0)
        .unwrap();
    assert_eq!(
        (opens(&h, MAXINE_MODULE), opens(&h, NPU)),
        (1, 2),
        "its table changed: the NPU module alone is opened again"
    );
    h.reload(
        &format!("[effect]\nmaxine_root = \"/opt/m\"\n[module.\"{NPU}\"]\nthreads = 2\n"),
        0,
    )
    .unwrap();
    assert_eq!((opens(&h, MAXINE_MODULE), opens(&h, NPU)), (2, 2));
    h.world().modules = vec![(manifest("2"), opened)];
    h.reload(
        &format!("[effect]\nmaxine_root = \"/opt/m\"\n[module.\"{NPU}\"]\nthreads = 2\n"),
        0,
    )
    .unwrap();
    assert_eq!(
        (opens(&h, MAXINE_MODULE), opens(&h, NPU)),
        (2, 3),
        "its manifest changed"
    );
}

#[test]
fn the_sdk_root_alias_and_the_module_table_reach_the_module() {
    let h = H::new(
        "[effect]\nmaxine_root = \"/opt/maxine\"\n[module.\"org.example.other\"]\nx = 1\n",
        |_| {},
    );
    let opens = h.world().module_opens.clone();
    assert_eq!(opens[0].1["sdk_root"], "/opt/maxine");
    drop(h);
    let h = H::new(
        "[effect]\nmaxine_root = \"/opt/maxine\"\n\
         [module.\"br.com.dias.maxine-vfx\"]\nsdk_root = \"/opt/other\"\n",
        |_| {},
    );
    let opens = h.world().module_opens.clone();
    assert_eq!(
        opens[0].1["sdk_root"], "/opt/other",
        "the module's own wins"
    );
}

#[test]
fn a_module_from_a_modules_dir_shows_its_group_and_a_write_reaches_its_stage() {
    let dir = hello_modules_dir();
    let mut h = H::new(&modules_config(dir.path()), |w| w.real_modules = true);
    let list = h.modules();
    let hello = list.find("org.example.hello").expect("discovered");
    assert_eq!(
        (hello.status.as_str(), hello.reason.as_str()),
        ("available", "")
    );
    assert_eq!(hello.stages[0].id, "hello");
    assert!(hello.stages[0].selected);
    assert_eq!(
        h.global_groups(),
        ["background", "video", "lighting", "hello"]
    );
    let on = h.setting("effect.hello.enabled");
    assert_eq!(on.kind.value(), Some(SettingValue::Bool(false)));
    h.set_setting("effect.hello.enabled", SettingValue::Bool(true), 0)
        .unwrap();
    h.activate(100);
    {
        let w = h.world();
        let (params, _) = w.session.as_ref().unwrap();
        assert_eq!(params.backends.chains.len(), 1);
        assert_eq!(params.backends.chains[0].1.id, "org.example.hello");
        assert!(params.backends.chains[0].1.module.is_some(), "loaded");
        let chain = &params.effect.chains[0];
        assert_eq!(chain.placement, Placement::AfterEffect);
        assert_eq!(chain.stages[0].id.as_str(), "hello");
        assert!(chain.stages[0].enabled);
    }
    // A live parameter reaches the running stage, without a reload.
    let default = match h.setting("effect.hello.strength").kind {
        SettingKind::Int { default, .. } => default,
        other => panic!("{other:?}"),
    };
    let new = if default == 30 { 40 } else { 30 };
    h.set_setting("effect.hello.strength", SettingValue::Int(new), 500)
        .unwrap();
    let w = h.world();
    let last = w.effect_sets.last().expect("given to the session");
    let stage = last.stage("hello").unwrap();
    assert_eq!(
        stage.live.get("strength"),
        Some(&obc_pipeline::effect::Value::Int(new))
    );
    assert!(w.stage_status.is_empty() || w.stage_status.contains_key("hello"));
}

#[test]
fn broken_missing_and_mismatched_modules_are_listed_with_their_reason() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = |id: &str, abi: u32, library: &str, extra: &str| {
        format!(
            "abi = {abi}\nid = \"{id}\"\nname = \"N\"\nversion = \"1\"\nlicence = \"MIT\"\n\
             kind = \"video-stage\"\nlibrary = \"{library}\"\n{extra}\
             [[stage]]\nid = \"tint\"\nslot = \"post-composite\"\nrank = 1\ndevice = \"cpu\"\n\
             frames = [\"host\"]\nformats = [\"nv12\"]\n"
        )
    };
    fs::write(
        dir.path().join("missing.toml"),
        manifest("org.x.missing", 1, "/nonexistent/libx.so", ""),
    )
    .unwrap();
    fs::write(
        dir.path().join("abi.toml"),
        manifest("org.x.future", 2, "/nonexistent/liby.so", ""),
    )
    .unwrap();
    fs::write(
        dir.path().join("typo.toml"),
        manifest(
            "org.x.typo",
            1,
            "/nonexistent/libz.so",
            "colour = \"red\"\n",
        ),
    )
    .unwrap();
    let mut h = H::new(&modules_config(dir.path()), |w| w.real_modules = true);
    let list = h.modules();
    let reason = |id: &str| {
        let m = list.find(id).unwrap_or_else(|| panic!("{id} listed"));
        assert_eq!(m.status, "unavailable", "{id}");
        assert!(m.enabled, "{id}");
        m.reason.clone()
    };
    assert!(reason("org.x.missing").contains("/nonexistent/libx.so"));
    assert!(
        reason("org.x.future").contains("ABI"),
        "{}",
        reason("org.x.future")
    );
    assert!(reason("org.x.typo").contains("colour"));
    // The daemon is up, the other module unaffected.
    assert_eq!(h.global_groups(), ["background", "video", "lighting"]);
    h.activate(100);
}

/// The crash ledger marks only the modules whose code the session runs: a
/// plain video session (no effect, no stage on) marks none; turning the
/// effect on marks its module before it runs, until the session stops.
#[test]
fn a_plain_video_session_marks_no_module_in_the_crash_ledger() {
    let mut h = H::new("", |_| {});
    let ledger = h.paths.state.with_file_name(LEDGER_FILE);
    fs::create_dir_all(ledger.parent().unwrap()).unwrap();
    h.paths.ledger = Some(ledger.clone());
    h.restart();
    let running = || CrashLedger::open(&ledger).is_running(MAXINE_MODULE);
    h.activate(0);
    assert!(!running(), "plain video runs no module code");
    h.call(|r| Command::SetEffect("blur".into(), r), 1000)
        .unwrap();
    assert!(running(), "the effect runs the module's code");
    h.call(|r| Command::SetEffect("none".into(), r), 1100)
        .unwrap();
    assert!(running(), "kept while the session runs");
    h.consumers(0, 2000);
    h.tick(8000);
    assert_eq!(h.state(), State::Idle);
    assert!(!running(), "cleared when the session stops");
    // A session that starts with a stage on marks it from the start.
    h.set_setting("effect.video.denoise", SettingValue::Bool(true), 9000)
        .unwrap();
    h.activate(10_000);
    assert!(running());
}

#[test]
fn a_module_that_crashed_the_daemon_twice_in_a_row_is_disabled_until_enabled() {
    let mut h = H::new("", |_| {});
    let ledger = h.paths.state.with_file_name(LEDGER_FILE);
    fs::create_dir_all(ledger.parent().unwrap()).unwrap();
    h.paths.ledger = Some(ledger.clone());
    for start in 1..=2 {
        // The daemon dies while the module's pipeline code runs.
        let mut l = CrashLedger::open(&ledger);
        l.begin(MAXINE_MODULE).unwrap();
        drop(l);
        h.restart();
        let m = h.modules().find(MAXINE_MODULE).cloned().unwrap();
        if start == 1 {
            assert!(m.enabled, "one crash is not enough");
            assert_eq!(h.global_groups(), ["background", "video", "lighting"]);
        } else {
            assert!(!m.enabled);
            assert_eq!(
                (m.status.as_str(), m.reason.as_str()),
                ("disabled", REASON_CRASHED)
            );
            assert!(h.global_groups().is_empty(), "its settings are gone");
        }
    }
    // A clean session clears the marker; re-enabling forgives it.
    h.call(
        |r| Command::SetModuleEnabled(MAXINE_MODULE.into(), true, r),
        0,
    )
    .unwrap();
    let m = h.modules().find(MAXINE_MODULE).cloned().unwrap();
    assert!(m.enabled && m.status == "available");
    assert_eq!(h.global_groups(), ["background", "video", "lighting"]);
    assert_eq!(CrashLedger::open(&ledger).disabled(MAXINE_MODULE), None);
    h.restart();
    assert!(h.modules().find(MAXINE_MODULE).unwrap().enabled, "kept");
}

#[test]
fn disabling_a_module_applies_at_the_next_start_and_is_kept() {
    let mut h = H::new("", |_| {});
    h.activate(0);
    let serial = h.modules().serial;
    h.call(
        |r| Command::SetModuleEnabled(MAXINE_MODULE.into(), false, r),
        500,
    )
    .unwrap();
    let list = h.modules();
    assert!(list.serial > serial);
    assert!(!list.find(MAXINE_MODULE).unwrap().enabled);
    assert!(h.global_groups().is_empty());
    // The running session keeps its modules.
    {
        let w = h.world();
        let (params, _) = w.session.as_ref().unwrap();
        assert!(params.backends.effect.is_some());
    }
    assert_eq!(
        h.managed_key(&format!("modules.enabled.{MAXINE_MODULE}")),
        Some(toml::Value::Boolean(false))
    );
    assert!(h.persisted().modules.is_empty(), "not in the state file");
    h.restart();
    assert!(!h.modules().find(MAXINE_MODULE).unwrap().enabled);
    assert!(matches!(
        h.call(
            |r| Command::SetModuleEnabled("org.x.none".into(), true, r),
            0
        ),
        Err(IpcError::InvalidArgument(_))
    ));
}

#[test]
fn the_stage_order_is_validated_kept_and_reset() {
    let mut h = H::new("", |_| {});
    assert_eq!(
        h.ctrl.order_ids(),
        ["enhance", "denoise", "background", "relight"]
    );
    let order = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let e = h
        .call(
            |r| Command::SetStageOrder(order(&["enhance", "relight", "denoise", "background"]), r),
            0,
        )
        .unwrap_err();
    match e {
        IpcError::InvalidArgument(m) => assert!(m.contains("relight"), "{m}"),
        other => panic!("{other:?}"),
    }
    let e = h
        .call(|r| Command::SetStageOrder(order(&["denoise"]), r), 0)
        .unwrap_err();
    assert!(e.message().contains("leaves out"), "{e:?}");
    h.call(
        |r| Command::SetStageOrder(order(&["denoise", "enhance", "background", "relight"]), r),
        0,
    )
    .unwrap();
    assert_eq!(
        h.ctrl.order_ids(),
        ["denoise", "enhance", "background", "relight"]
    );
    assert!(h.managed_key("modules.order").is_some());
    assert!(h.persisted().modules.is_empty(), "not in the state file");
    h.restart();
    assert_eq!(
        h.ctrl.order_ids(),
        ["denoise", "enhance", "background", "relight"]
    );
    h.call(|r| Command::SetStageOrder(Vec::new(), r), 0)
        .unwrap();
    assert_eq!(
        h.ctrl.order_ids(),
        ["enhance", "denoise", "background", "relight"]
    );
    assert_eq!(h.managed_key("modules.order"), None);
}

#[test]
fn a_stored_order_that_no_longer_fits_falls_back_to_the_default() {
    let mut h = H::new("", |_| {});
    let mut st = RuntimeState::default();
    st.modules.order = Some(vec!["relight".into(), "background".into()]);
    st.save(&h.paths.state).unwrap();
    h.restart();
    assert_eq!(
        h.ctrl.order_ids(),
        ["enhance", "denoise", "background", "relight"]
    );
}

#[test]
fn the_profile_and_pins_are_reported_in_the_module_list() {
    let mut h = H::new("", |w| {
        w.modules = vec![(
            crate::modules::testing::entry(
                "abi = 1\nid = \"org.example.npu\"\nname = \"NPU cut-out\"\nversion = \"1\"\n\
                 licence = \"MIT\"\nkind = \"video-stage\"\nlibrary = \"/m/libnpu.so\"\n\
                 [[stage]]\nid = \"background\"\nslot = \"matte\"\nrank = 100\n\
                 device = \"npu\"\nframes = [\"host\"]\nformats = [\"nv12\"]\n",
                "/m/npu.toml",
            ),
            Ok(crate::backend::OpenedModule {
                module: None,
                stages: vec![(
                    StageId::BACKGROUND,
                    obc_pipeline::effect::Availability::Installed,
                )],
                settings: Default::default(),
            }),
        )];
    });
    let selected = |h: &mut H| {
        h.modules()
            .provider_of("background")
            .map(|m| m.id.clone())
            .unwrap()
    };
    assert_eq!(selected(&mut h), MAXINE_MODULE, "quality: the highest rank");
    // Every change of what `GetModules` (or `GetStageOrder`) returns moves
    // `ModulesSerial`, announced, and the list carries it.
    let mut serial = h.snap().modules_serial.unwrap();
    let mut moved = |h: &mut H, what: &str| {
        let now = h.snap().modules_serial.unwrap();
        assert_ne!(now, serial, "{what}: the serial moves");
        assert_eq!(h.modules().serial, now, "{what}");
        assert!(
            h.drain().0.contains(&Change::ModulesSerial(now)),
            "{what}: announced"
        );
        serial = now;
    };
    h.drain();
    h.call(|r| Command::SetBackendProfile("battery".into(), r), 0)
        .unwrap();
    assert_eq!(
        selected(&mut h),
        "org.example.npu",
        "battery: the NPU first"
    );
    moved(&mut h, "profile");
    assert_eq!(h.snap().backend_profile.as_deref(), Some("battery"));
    h.call(
        |r| Command::SetStagePin("background".into(), MAXINE_MODULE.into(), r),
        0,
    )
    .unwrap();
    assert_eq!(selected(&mut h), MAXINE_MODULE, "pinned");
    moved(&mut h, "pin");
    h.call(
        |r| Command::SetStagePin("background".into(), String::new(), r),
        0,
    )
    .unwrap();
    assert_eq!(selected(&mut h), "org.example.npu");
    moved(&mut h, "unpinned");
    let order = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    h.call(
        |r| Command::SetStageOrder(order(&["denoise", "enhance", "background", "relight"]), r),
        0,
    )
    .unwrap();
    moved(&mut h, "order");
    h.call(|r| Command::SetStageOrder(Vec::new(), r), 0)
        .unwrap();
    moved(&mut h, "default order");
    assert!(matches!(
        h.call(|r| Command::SetBackendProfile("fast".into(), r), 0),
        Err(IpcError::InvalidArgument(_))
    ));
    assert_eq!(
        h.managed_key("modules.profile"),
        Some(toml::Value::String("battery".into()))
    );
    assert_eq!(h.managed_key("modules.pin"), None, "the pin is removed");
    assert!(h.persisted().modules.is_empty(), "not in the state file");
}

#[test]
fn the_revision_8_methods_are_aliases_of_the_registry_settings() {
    let mut h = H::new("", |_| {});
    h.call(|r| Command::SetDenoise(true, r), 0).unwrap();
    assert_eq!(
        h.setting(DENOISE_ID).kind.value(),
        Some(SettingValue::Bool(true))
    );
    assert!(h.snap().denoise);
    h.call(|r| Command::SetEffect("blur".into(), r), 0).unwrap();
    h.call(|r| Command::SetBlurStrength(0.25, r), 0).unwrap();
    assert_eq!(h.setting(EFFECT_ID).kind.value(), Some(s("blur")));
    assert_eq!(h.setting(BLUR_ID).kind.value(), Some(SettingValue::Int(25)));
    let st = h.persisted();
    assert_eq!(st.denoise, Some(true), "kept by its setting id");
    assert_eq!(st.effect, Some(EffectMode::Blur));
    // And through the setting, the property.
    h.set_setting(DENOISE_ID, SettingValue::Bool(false), 100)
        .unwrap();
    assert!(!h.snap().denoise);
}

#[test]
fn a_revision_8_state_file_and_config_keep_their_meaning() {
    let mut h = H::new(
        "[effect]\nrelight = true\nrelight_brightness = 120\n",
        |_| {},
    );
    fs::create_dir_all(h.paths.state.parent().unwrap()).unwrap();
    fs::write(
        &h.paths.state,
        "enabled = true\neffect = \"blur\"\ndenoise = true\nrelight_light = \"warm\"\n\
         relight_direction = -45\n",
    )
    .unwrap();
    h.restart();
    let value = |h: &mut H, id: &str| h.setting(id).kind.value();
    assert_eq!(value(&mut h, EFFECT_ID), Some(s("blur")));
    assert_eq!(value(&mut h, DENOISE_ID), Some(SettingValue::Bool(true)));
    assert_eq!(value(&mut h, RELIGHT_ID), Some(SettingValue::Bool(true)));
    assert_eq!(value(&mut h, LIGHT_ID), Some(s("warm")));
    assert_eq!(value(&mut h, DIRECTION_ID), Some(SettingValue::Int(-45)));
    assert_eq!(value(&mut h, BRIGHTNESS_ID), Some(SettingValue::Int(120)));
    assert!(h.setting(DENOISE_ID).stored && !h.setting(RELIGHT_ID).stored);
    assert!(h.snap().denoise);
    h.activate(0);
    let applied = h.world().effect.clone().unwrap();
    assert!(applied.denoise() && applied.relight());
    assert_eq!(
        (
            applied.relight_light(),
            applied.relight_direction(),
            applied.relight_brightness()
        ),
        (RelightLight::Warm, -45, 120)
    );
}

// ---- the configuration set (config.d) and its writes (revision 10) ----

use obc_core::config_d::MANAGED_HEADER;

impl H {
    fn ask<T>(
        &mut self,
        make: impl FnOnce(tokio::sync::oneshot::Sender<Result<T, IpcError>>) -> Command,
    ) -> Result<T, IpcError> {
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let now = self.at(0);
        assert!(self.ctrl.handle(make(tx), now));
        rx.try_recv()
            .expect("the controller replies before returning")
    }

    fn schema(&mut self) -> obc_ipc::ConfigSchema {
        self.ask(Command::GetConfigSchema).unwrap()
    }

    fn row(&mut self, id: &str) -> obc_ipc::ConfigRow {
        self.schema()
            .find(id)
            .cloned()
            .unwrap_or_else(|| panic!("no row {id}"))
    }

    fn set_config(&mut self, key: &str, value: obc_ipc::ConfigValue) -> Result<(), IpcError> {
        let key = key.to_string();
        self.call(|r| Command::SetConfig(key, Some(value), r), 0)
    }

    fn config_d(&self, name: &str) -> std::path::PathBuf {
        self.paths.config.with_file_name("config.d").join(name)
    }

    fn write_drop_in(&self, name: &str, text: &str) {
        let path = self.config_d(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn write_state(&self, text: &str) {
        fs::create_dir_all(self.paths.state.parent().unwrap()).unwrap();
        fs::write(&self.paths.state, text).unwrap();
    }

    /// The state file's text.
    fn state_text(&self) -> String {
        fs::read_to_string(&self.paths.state).unwrap()
    }
}

/// A state file of 0.24 with the module choices (and a revision-8 typed
/// effect key).
const STATE_0_24: &str = "enabled = true\ndenoise = true\n\n[modules]\norder = \
     [\"denoise\", \"enhance\", \"background\", \"relight\"]\nprofile = \"battery\"\n\n\
     [modules.enabled]\n\"br.com.dias.maxine-vfx\" = false\n\n[modules.pin]\n\
     background = \"br.com.dias.maxine-vfx\"\n";

#[test]
fn the_state_files_module_choices_move_into_the_managed_file_losing_nothing() {
    let config = "[lifecycle]\nidle_grace_s = 7\n";
    let mut h = H::new(config, |_| {});
    h.write_state(STATE_0_24);
    h.restart();
    // Every choice is in the managed file, in force, and out of the state.
    assert_eq!(
        h.managed_key(&format!("modules.enabled.{MAXINE_MODULE}")),
        Some(toml::Value::Boolean(false))
    );
    assert_eq!(
        h.managed_key("modules.profile"),
        Some(toml::Value::String("battery".into()))
    );
    assert_eq!(
        h.managed_key("modules.pin.background"),
        Some(toml::Value::String(MAXINE_MODULE.into()))
    );
    assert!(h.managed_key("modules.order").is_some());
    let text = fs::read_to_string(h.config_d("50-panel.toml")).unwrap();
    assert!(text.starts_with(MANAGED_HEADER), "{text}");
    assert!(!h.modules().find(MAXINE_MODULE).unwrap().enabled);
    assert_eq!(h.snap().backend_profile.as_deref(), Some("battery"));
    let state = h.state_text();
    assert!(!state.contains("[modules"), "{state}");
    // The rest of the state is kept, the revision-8 typed key written too.
    assert!(h.persisted().enabled);
    assert!(state.contains("denoise = true"), "{state}");
    assert_eq!(fs::read_to_string(&h.paths.config).unwrap(), config);
    // A second start moves nothing more.
    let before = fs::read(h.config_d("50-panel.toml")).unwrap();
    h.restart();
    assert_eq!(fs::read(h.config_d("50-panel.toml")).unwrap(), before);
    assert!(!h.modules().find(MAXINE_MODULE).unwrap().enabled);
}

#[test]
fn a_crash_between_the_two_writes_of_the_move_loses_nothing() {
    let mut h = H::new("", |_| {});
    // The managed file was written, the daemon died before saving the
    // state: both hold the choices (the managed file's profile differs,
    // as a write after the move would leave it).
    h.write_drop_in(
        "50-panel.toml",
        &format!(
            "{MANAGED_HEADER}\n[modules]\nprofile = \"quality\"\norder = [\"denoise\", \
             \"enhance\", \"background\", \"relight\"]\n\n[modules.enabled]\n\
             \"{MAXINE_MODULE}\" = false\n\n[modules.pin]\nbackground = \"{MAXINE_MODULE}\"\n"
        ),
    );
    let managed = fs::read(h.config_d("50-panel.toml")).unwrap();
    h.write_state(STATE_0_24);
    h.restart();
    assert_eq!(
        fs::read(h.config_d("50-panel.toml")).unwrap(),
        managed,
        "the managed file wins: nothing to write"
    );
    assert!(
        !h.state_text().contains("[modules"),
        "the state's copy is dropped"
    );
    assert_eq!(h.snap().backend_profile.as_deref(), Some("quality"));
    // Disabled by the managed file: no stage is offered.
    assert!(!h.modules().find(MAXINE_MODULE).unwrap().enabled);
    assert!(h.ctrl.order_ids().is_empty());
}

#[cfg(unix)]
#[test]
fn a_move_that_cannot_be_written_keeps_the_state_and_is_tried_again() {
    use std::os::unix::fs::PermissionsExt;
    let mut h = H::new("", |_| {});
    let dir = h.paths.config.with_file_name("config.d");
    fs::create_dir_all(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
    h.write_state(STATE_0_24);
    h.restart();
    let writable = fs::write(dir.join("probe"), "").is_ok();
    if !writable {
        assert_eq!(h.state_text(), STATE_0_24, "the state is left as it was");
        assert!(!dir.join("50-panel.toml").exists());
        // The state's choices stay in force meanwhile, as before the move.
        assert_eq!(h.snap().backend_profile.as_deref(), Some("battery"));
        assert!(!h.modules().find(MAXINE_MODULE).unwrap().enabled);
        // A module method cannot write: refused with the reason, nothing
        // changed.
        let e = h
            .call(|r| Command::SetBackendProfile("quality".into(), r), 0)
            .unwrap_err();
        assert!(matches!(e, IpcError::Failed(_)), "{e:?}");
        assert!(e.message().contains("50-panel.toml"), "{e:?}");
        assert_eq!(h.snap().backend_profile.as_deref(), Some("battery"));
        // A save of the state keeps the choices it holds.
        h.call(|r| Command::SetEnabled(true, r), 0).unwrap();
        assert!(h.state_text().contains("[modules"));
        // A reload that cannot move them either keeps them in force.
        h.call(|r| Command::ReloadConfig(Some(r)), 0).unwrap();
        assert_eq!(h.snap().backend_profile.as_deref(), Some("battery"));
        assert!(!h.modules().find(MAXINE_MODULE).unwrap().enabled);
    }
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    h.call(|r| Command::ReloadConfig(Some(r)), 0).unwrap();
    assert!(!h.state_text().contains("[modules"));
    assert_eq!(
        h.managed_key("modules.profile"),
        Some(toml::Value::String("battery".into()))
    );
    assert_eq!(h.snap().backend_profile.as_deref(), Some("battery"));
}

#[test]
fn writes_land_in_the_managed_file_only_and_announce_the_config_serial() {
    let config = "# mine\n[output]\nhold_timeout_s = 3.0\n";
    let mut h = H::new(config, |_| {});
    let serial = h.snap().config_serial.unwrap();
    h.drain();
    h.set_config("output.hold_timeout_s", obc_ipc::ConfigValue::Int(9))
        .unwrap();
    let row = h.row("output.hold_timeout_s");
    // A whole number is taken for a float key.
    assert_eq!(row.value, obc_ipc::ConfigValue::Float(9.0));
    assert_eq!(row.source, "config.d/50-panel.toml");
    assert_eq!(row.overridden_by, "");
    assert_eq!(h.ctrl.config().output.hold_timeout_s, 9.0);
    let now = h.snap().config_serial.unwrap();
    assert_ne!(now, serial);
    assert!(h.drain().0.contains(&Change::ConfigSerial(now)));
    assert_eq!(h.config_notes.last(), Some(&now), "ConfigChanged follows");
    // The module methods write the same file.
    h.call(
        |r| Command::SetModuleEnabled(MAXINE_MODULE.into(), false, r),
        0,
    )
    .unwrap();
    h.call(|r| Command::SetBackendProfile("battery".into(), r), 0)
        .unwrap();
    h.call(
        |r| Command::SetStagePin("background".into(), MAXINE_MODULE.into(), r),
        0,
    )
    .unwrap();
    let text = fs::read_to_string(h.config_d("50-panel.toml")).unwrap();
    assert!(text.starts_with(MANAGED_HEADER), "{text}");
    assert_eq!(
        h.managed_key("output.hold_timeout_s"),
        Some(toml::Value::Float(9.0))
    );
    assert!(h.managed_key("modules.pin.background").is_some());
    assert_eq!(fs::read_to_string(&h.paths.config).unwrap(), config);
    assert!(
        !h.paths.state.exists() || !h.state_text().contains("[modules"),
        "no module choice in the state file"
    );
    let entries: Vec<String> = fs::read_dir(h.config_d(""))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(entries, ["50-panel.toml"], "no other file");
    // A reset removes the key: config.toml's value is back.
    let key = "output.hold_timeout_s".to_string();
    h.call(|r| Command::ResetConfig(key, r), 0).unwrap();
    assert_eq!(h.ctrl.config().output.hold_timeout_s, 3.0);
    assert_eq!(h.row("output.hold_timeout_s").source, "config.toml");
    // A write that changes nothing moves nothing.
    let serial = h.snap().config_serial.unwrap();
    h.call(|r| Command::SetBackendProfile("battery".into(), r), 0)
        .unwrap();
    assert_eq!(h.snap().config_serial.unwrap(), serial);
}

#[test]
fn a_key_a_later_file_sets_is_refused_with_the_file() {
    let mut h = H::new("", |_| {});
    h.write_drop_in(
        "90-local.toml",
        &format!(
            "[output]\nhold_timeout_s = 4.0\n[modules]\nprofile = \"quality\"\norder = \
             [\"enhance\", \"denoise\", \"background\", \"relight\"]\n[modules.enabled]\n\
             \"{MAXINE_MODULE}\" = true\n[modules.pin]\nbackground = \"\"\n"
        ),
    );
    h.call(|r| Command::ReloadConfig(Some(r)), 0).unwrap();
    let overridden = |e: Result<(), IpcError>| match e {
        Err(IpcError::Disabled(m)) => assert_eq!(m, "Overridden by config.d/90-local.toml"),
        other => panic!("{other:?}"),
    };
    overridden(h.set_config("output.hold_timeout_s", obc_ipc::ConfigValue::Int(9)));
    overridden(h.call(
        |r| Command::ResetConfig("output.hold_timeout_s".into(), r),
        0,
    ));
    overridden(h.call(|r| Command::SetBackendProfile("battery".into(), r), 0));
    overridden(h.call(
        |r| Command::SetModuleEnabled(MAXINE_MODULE.into(), false, r),
        0,
    ));
    overridden(h.call(
        |r| Command::SetStagePin("background".into(), MAXINE_MODULE.into(), r),
        0,
    ));
    overridden(h.call(|r| Command::SetStageOrder(Vec::new(), r), 0));
    assert!(!h.config_d("50-panel.toml").exists(), "nothing written");
    let row = h.row("output.hold_timeout_s");
    assert_eq!(
        (row.overridden_by.as_str(), row.enabled, row.source.as_str()),
        ("config.d/90-local.toml", false, "config.d/90-local.toml")
    );
    assert_eq!(row.disabled_reason, "Overridden by config.d/90-local.toml");
}

#[test]
fn a_reload_takes_a_hand_written_drop_in_and_names_a_bad_one() {
    let mut h = H::new("[lifecycle]\nidle_grace_s = 7\n", |_| {});
    let serial = h.snap().config_serial.unwrap();
    h.write_drop_in("90-mine.toml", "[lifecycle]\nidle_grace_s = 9\n");
    h.call(|r| Command::ReloadConfig(Some(r)), 0).unwrap();
    assert_eq!(h.ctrl.config().idle_grace(), Duration::from_secs(9));
    assert_eq!(
        h.row("lifecycle.idle_grace_s").source,
        "config.d/90-mine.toml"
    );
    assert_ne!(h.snap().config_serial.unwrap(), serial);
    // A bad drop-in: refused, named with its line; the set stays.
    h.write_drop_in("95-bad.toml", "\n[lifecycle]\nidle_grace = 1\n");
    let e = h.call(|r| Command::ReloadConfig(Some(r)), 0).unwrap_err();
    let msg = e.message().to_string();
    assert!(matches!(e, IpcError::InvalidConfig(_)), "{e:?}");
    assert!(
        msg.contains("config.d/95-bad.toml") && msg.contains(":3"),
        "{msg}"
    );
    assert_eq!(h.snap().error, msg);
    assert_eq!(h.ctrl.config().idle_grace(), Duration::from_secs(9));
    let files = h.ask(Command::GetConfigFiles).unwrap();
    let bad = files
        .files
        .iter()
        .find(|f| f.path == "config.d/95-bad.toml")
        .unwrap();
    assert_eq!(bad.status, "error");
    assert!(!bad.reason.is_empty());
    let managed = files.files.iter().find(|f| f.managed).unwrap();
    assert_eq!(
        (managed.path.as_str(), managed.status.as_str()),
        ("config.d/50-panel.toml", "absent")
    );
    assert_eq!(files.files[0].path, "config.toml");
    assert_eq!(files.serial, h.snap().config_serial.unwrap());
    // A write while a file is bad is refused, naming it.
    let e = h
        .set_config("lifecycle.idle_grace_s", obc_ipc::ConfigValue::Int(2))
        .unwrap_err();
    assert!(matches!(e, IpcError::InvalidConfig(_)), "{e:?}");
}

#[test]
fn set_config_refuses_unknown_read_only_and_invalid_values() {
    let mut h = H::new("", |_| {});
    use obc_ipc::ConfigValue as V;
    assert!(matches!(
        h.set_config("output.nope", V::Int(1)),
        Err(IpcError::UnknownSetting(_))
    ));
    assert!(matches!(
        h.set_config("modules.enabled.org.x.none", V::Bool(true)),
        Err(IpcError::UnknownSetting(_))
    ));
    assert!(matches!(
        h.set_config("camera.mode", V::Str("1280x720@30".into())),
        Err(IpcError::Disabled(_))
    ));
    // Not one of the output modes offered.
    assert!(matches!(
        h.set_config("output.mode", V::Str("1281x720@30".into())),
        Err(IpcError::InvalidArgument(_))
    ));
    assert!(matches!(
        h.set_config("output.hold_timeout_s", V::Str("x".into())),
        Err(IpcError::InvalidArgument(_))
    ));
    assert!(matches!(
        h.call(
            |r| Command::SetConfig("output.hold_timeout_s".into(), None, r),
            0
        ),
        Err(IpcError::InvalidArgument(_))
    ));
    assert!(!h.config_d("50-panel.toml").exists());
    // A module row through SetConfig is the module method.
    h.set_config(&format!("modules.enabled.{MAXINE_MODULE}"), V::Bool(false))
        .unwrap();
    assert!(!h.modules().find(MAXINE_MODULE).unwrap().enabled);
    assert!(h.global_groups().is_empty());
    h.set_config("modules.profile", V::Str("battery".into()))
        .unwrap();
    assert_eq!(h.snap().backend_profile.as_deref(), Some("battery"));
}

#[test]
fn the_schema_has_the_module_rows_and_follows_the_modules_found() {
    let mut h = H::new("", |_| {});
    let schema = h.schema();
    let ids: Vec<&str> = schema.groups.iter().map(|g| g.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "camera",
            "output",
            "loopback",
            "lifecycle",
            "effect",
            "pipewire",
            "modules"
        ]
    );
    let row = h.row(&format!("modules.enabled.{MAXINE_MODULE}"));
    assert_eq!(row.value, obc_ipc::ConfigValue::Bool(true));
    let pin = h.row("modules.pin.background");
    match &pin.kind {
        obc_ipc::ConfigKind::Enum { options } => {
            assert_eq!(options[0].0, "");
            assert!(options.iter().any(|(v, _)| v == MAXINE_MODULE));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(schema.serial, h.snap().config_serial.unwrap());
}

#[test]
fn the_order_moves_are_those_the_order_check_accepts() {
    let mut h = H::new("", |_| {});
    let moves = h.ask(Command::GetStageOrderMoves).unwrap();
    let ids: Vec<&str> = moves.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["enhance", "denoise", "background", "relight"]);
    assert!(!moves[0].up && !moves[3].down, "no neighbour");
    // enhance and denoise may swap (an order the method accepts).
    assert!(moves[0].down && moves[1].up);
    assert_eq!(moves[2].slot, "matte");
    let order = h.ctrl.order_ids();
    for i in 0..moves.len() - 1 {
        let mut o = order.clone();
        o.swap(i, i + 1);
        let accepted = h.call(|r| Command::SetStageOrder(o, r), 0).is_ok();
        assert_eq!(accepted, moves[i].down, "{i} down");
        assert_eq!(accepted, moves[i + 1].up, "{} up", i + 1);
        h.call(|r| Command::SetStageOrder(Vec::new(), r), 0)
            .unwrap();
    }
}

#[cfg(unix)]
#[test]
fn a_write_once_the_directory_is_writable_moves_the_waiting_choices_first() {
    use std::os::unix::fs::PermissionsExt;
    let mut h = H::new("", |_| {});
    let dir = h.paths.config.with_file_name("config.d");
    fs::create_dir_all(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
    h.write_state(STATE_0_24);
    h.restart();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    // No reload: the write moves the state's choices, then goes over them.
    h.call(|r| Command::SetBackendProfile("quality".into(), r), 0)
        .unwrap();
    assert_eq!(
        h.managed_key("modules.profile"),
        Some(toml::Value::String("quality".into()))
    );
    assert_eq!(
        h.managed_key(&format!("modules.enabled.{MAXINE_MODULE}")),
        Some(toml::Value::Boolean(false))
    );
    assert!(h.managed_key("modules.pin.background").is_some());
    assert!(!h.state_text().contains("[modules"));
    assert_eq!(h.snap().backend_profile.as_deref(), Some("quality"));
    assert!(!h.modules().find(MAXINE_MODULE).unwrap().enabled);
}

#[test]
fn a_set_that_does_not_load_at_start_keeps_the_states_choices_in_force() {
    let mut h = H::new("", |_| {});
    fs::write(&h.paths.config, "[output]\nnope = 1\n").unwrap();
    h.write_state(STATE_0_24);
    h.restart();
    assert!(!h.snap().error.is_empty());
    // Not moved (nothing to write them into), in force from the state.
    assert!(!h.config_d("50-panel.toml").exists());
    assert_eq!(h.state_text(), STATE_0_24);
    assert_eq!(h.snap().backend_profile.as_deref(), Some("battery"));
    assert!(!h.modules().find(MAXINE_MODULE).unwrap().enabled);
    // A write names the file that does not load.
    let e = h
        .call(|r| Command::SetBackendProfile("quality".into(), r), 0)
        .unwrap_err();
    assert!(matches!(e, IpcError::InvalidConfig(_)), "{e:?}");
    assert_eq!(h.snap().backend_profile.as_deref(), Some("battery"));
    // The first reload that succeeds moves them.
    h.reload("", 0).unwrap();
    assert!(!h.state_text().contains("[modules"));
    assert_eq!(
        h.managed_key("modules.profile"),
        Some(toml::Value::String("battery".into()))
    );
    assert_eq!(h.snap().backend_profile.as_deref(), Some("battery"));
    assert!(!h.modules().find(MAXINE_MODULE).unwrap().enabled);
}

#[test]
fn no_order_move_is_offered_when_a_later_file_sets_the_order() {
    let mut h = H::new("", |_| {});
    let moves = h.ask(Command::GetStageOrderMoves).unwrap();
    assert!(moves.iter().any(|m| m.up || m.down));
    h.write_drop_in(
        "90-local.toml",
        "[modules]\norder = [\"enhance\", \"denoise\", \"background\", \"relight\"]\n",
    );
    h.call(|r| Command::ReloadConfig(Some(r)), 0).unwrap();
    let moves = h.ask(Command::GetStageOrderMoves).unwrap();
    assert_eq!(moves.len(), 4);
    assert!(moves.iter().all(|m| !m.up && !m.down), "{moves:?}");
    // As SetStageOrder refuses every order.
    let mut o = h.ctrl.order_ids();
    o.swap(0, 1);
    assert!(matches!(
        h.call(|r| Command::SetStageOrder(o, r), 0),
        Err(IpcError::Disabled(_))
    ));
    // The managed file's own order leaves the moves.
    fs::remove_file(h.config_d("90-local.toml")).unwrap();
    h.write_drop_in(
        "50-panel.toml",
        "[modules]\norder = [\"enhance\", \"denoise\", \"background\", \"relight\"]\n",
    );
    h.call(|r| Command::ReloadConfig(Some(r)), 0).unwrap();
    let moves = h.ask(Command::GetStageOrderMoves).unwrap();
    assert!(moves[0].down && moves[1].up);
}

#[test]
fn a_stale_managed_entry_for_a_module_no_longer_installed_is_reset() {
    let mut h = H::new("", |_| {});
    h.write_drop_in(
        "50-panel.toml",
        "[modules.enabled]\n\"org.gone.module\" = false\n[modules.pin]\nhaze = \"org.gone.module\"\n",
    );
    h.call(|r| Command::ReloadConfig(Some(r)), 0).unwrap();
    for key in ["modules.enabled.org.gone.module", "modules.pin.haze"] {
        let k = key.to_string();
        h.call(|r| Command::ResetConfig(k, r), 0)
            .unwrap_or_else(|e| panic!("{key}: {e:?}"));
        assert_eq!(h.managed_key(key), None, "{key}");
    }
    assert!(matches!(
        h.call(|r| Command::ResetConfig("modules.pin.haze".into(), r), 0),
        Err(IpcError::UnknownSetting(_))
    ));
}

#[test]
fn enabling_a_module_that_fails_to_open_is_kept_and_says_why_in_its_status() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("missing.toml"),
        "abi = 1\nid = \"org.x.missing\"\nname = \"N\"\nversion = \"1\"\nlicence = \"MIT\"\n\
         kind = \"video-stage\"\nlibrary = \"/nonexistent/libx.so\"\n\
         [[stage]]\nid = \"tint\"\nslot = \"post-composite\"\nrank = 1\ndevice = \"cpu\"\n\
         frames = [\"host\"]\nformats = [\"nv12\"]\n",
    )
    .unwrap();
    let mut h = H::new(&modules_config(dir.path()), |w| w.real_modules = true);
    h.call(
        |r| Command::SetModuleEnabled("org.x.missing".into(), false, r),
        0,
    )
    .unwrap();
    // As in 0.24: an open that fails is no refusal; the status says why.
    h.call(
        |r| Command::SetModuleEnabled("org.x.missing".into(), true, r),
        0,
    )
    .unwrap();
    assert_eq!(
        h.managed_key("modules.enabled.org.x.missing"),
        Some(toml::Value::Boolean(true))
    );
    let list = h.modules();
    let m = list.find("org.x.missing").unwrap();
    assert!(m.enabled);
    assert_eq!(m.status, "unavailable");
    assert!(m.reason.contains("/nonexistent/libx.so"), "{}", m.reason);
}

// ---- a stage no module provides is dropped, the feed kept ----

impl H {
    fn set_module(&mut self, id: &str, on: bool, ms: u64) {
        let id = id.to_string();
        self.call(|r| Command::SetModuleEnabled(id, on, r), ms)
            .unwrap();
    }

    /// The effect the running session was last given.
    fn last_effect(&self) -> EffectRun {
        self.world()
            .effect_sets
            .last()
            .cloned()
            .expect("an effect was given")
    }
}

fn stage_on(effect: &EffectRun, id: &str) -> bool {
    effect.stage(id).is_some_and(|s| s.enabled)
}

#[test]
fn disabling_the_effect_module_drops_its_stages_and_keeps_the_feed_and_the_others() {
    let dir = hello_modules_dir();
    let config = format!(
        "{}[effect]\nmode = \"blur\"\ndenoise = true\n",
        modules_config(dir.path())
    );
    let mut h = H::new(&config, |w| w.real_modules = true);
    h.set_setting("effect.hello.enabled", SettingValue::Bool(true), 0)
        .unwrap();
    h.activate(100);
    assert_eq!(h.applied(), Some(EffectMode::Blur));
    assert_eq!(h.snap().warning, "");
    let ingests = h.world().ingests.len();
    h.drain();
    h.set_module(MAXINE_MODULE, false, 1000);
    // The session runs on, the camera open, without the effect module's
    // stages; the other module's stage still runs.
    assert_eq!(h.state(), State::Active);
    {
        let w = h.world();
        assert_eq!(w.session_starts, 1);
        assert_eq!(w.session_stops, 0);
        assert_eq!(w.ingests.len(), ingests, "the camera is not reopened");
    }
    let last = h.last_effect();
    assert_eq!(last.mode, EffectMode::None);
    assert!(!stage_on(&last, "denoise") && !stage_on(&last, "relight"));
    assert!(stage_on(&last, "hello"), "{last:?}");
    // Every selected stage of the disabled module, in one clause.
    let dropped = "effect blur, noise removal unavailable: NVIDIA Maxine video effects: Disabled";
    let snap = h.snap();
    assert_eq!((snap.error.as_str(), snap.warning.as_str()), ("", dropped));
    assert_eq!(snap.effect, "blur", "the choice is kept");
    assert!(h.drain().0.contains(&Change::Warning(dropped.into())));
    // The idle grace and polls change nothing.
    h.tick(1500);
    h.tick(5000);
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.snap().error, "");
    // Re-enabled: the stages come back in the same session, with no call
    // for the effect, and the warning clears.
    h.set_module(MAXINE_MODULE, true, 6000);
    let last = h.last_effect();
    assert_eq!(last.mode, EffectMode::Blur);
    assert!(stage_on(&last, "denoise") && stage_on(&last, "hello"));
    {
        let w = h.world();
        assert_eq!(w.session_starts, 1, "no restart");
        assert_eq!(w.ingests.len(), ingests, "the camera is not reopened");
    }
    let snap = h.snap();
    assert_eq!((snap.error.as_str(), snap.warning.as_str()), ("", ""));
    assert_eq!(h.state(), State::Active);
    assert!(h.drain().0.contains(&Change::Warning(String::new())));
    assert_eq!(h.snap().effect, "blur");
    assert_eq!(h.persisted().effect, None, "nothing was written");
}

#[test]
fn a_session_started_with_its_effect_module_disabled_streams_and_restarts_once_enabled() {
    // `on_failure = "black"` (the default) does not apply: nothing failed.
    let mut h = H::new(
        "[effect]\nmode = \"blur\"\non_failure = \"black\"\n",
        |_| {},
    );
    h.set_module(MAXINE_MODULE, false, 0);
    h.activate(100);
    let snap = h.snap();
    assert_eq!((snap.error.as_str(), snap.warning.as_str()), ("", NO_BLUR));
    {
        let w = h.world();
        let (params, _) = w.session.as_ref().unwrap();
        assert_eq!(params.effect.mode, EffectMode::None, "passthrough");
        assert!(params.backends.effect.is_none());
    }
    // The same after the camera stops and starts again (the case that
    // used to end in `error`).
    h.consumers(0, 1000);
    h.tick(60_000);
    assert!(h.world().session.is_none());
    h.activate(61_000);
    assert_eq!(h.snap().warning, NO_BLUR);
    assert_eq!(h.world().session_starts, 2);
    // Enabled again: the session, built without it, is started again with
    // it, by itself.
    h.set_module(MAXINE_MODULE, true, 62_000);
    h.tick(62_400);
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.world().session_starts, 3);
    assert_eq!(h.applied(), Some(EffectMode::Blur));
    {
        let w = h.world();
        let (params, _) = w.session.as_ref().unwrap();
        assert_eq!(params.effect.mode, EffectMode::Blur);
        assert!(params.backends.effect.is_some());
    }
    let snap = h.snap();
    assert_eq!((snap.error.as_str(), snap.warning.as_str()), ("", ""));
    assert_eq!(snap.effect, "blur");
}

#[test]
fn a_runtime_failure_of_a_provided_effect_follows_on_failure_and_never_stops_the_feed() {
    // `black`: the element's frames black, `Warning` says why.
    let mut h = H::new("[effect]\nmode = \"blur\"\n", |w| w.effect_slow = true);
    h.activate(0);
    h.world().effect_status = EffectStatus::Ready;
    h.tick(500);
    h.world().effect_status = EffectStatus::Failed("processing a frame failed".into());
    h.tick(1000);
    assert_eq!(h.state(), State::Active);
    let snap = h.snap();
    assert_eq!(snap.error, "");
    assert_eq!(snap.warning, black("processing a frame failed"));
    assert!(h.world().session.is_some());
    assert_eq!(h.world().effect.as_ref().unwrap().mode, EffectMode::Blur);
    // Its module disabled meanwhile: the effect is dropped (no module
    // provides it) and the feed shows the camera, as for any stage no
    // module provides; the failure is no longer what it shows.
    h.set_module(MAXINE_MODULE, false, 2000);
    assert_eq!(h.last_effect().mode, EffectMode::None);
    assert_eq!(h.snap().warning, NO_BLUR);
    // Enabled again: given again, a retry (it runs now).
    h.world().effect_slow = false;
    h.set_module(MAXINE_MODULE, true, 3000);
    assert_eq!(h.last_effect().mode, EffectMode::Blur);
    assert_eq!(h.world().effect_status, EffectStatus::Ready);
    let snap = h.snap();
    assert_eq!((snap.error.as_str(), snap.warning.as_str()), ("", ""));
    assert_eq!(h.world().session_starts, 1);
    // `none`: the camera image, the reason in `Warning`, nothing in
    // `Error`.
    let mut h = H::new("[effect]\nmode = \"blur\"\non_failure = \"none\"\n", |w| {
        w.effect_fail = Some("the model did not load".into())
    });
    h.activate(0);
    assert_eq!(h.state(), State::Active);
    assert_eq!(
        h.world().effect.as_ref().unwrap().on_failure,
        EffectOnFailure::None
    );
    let snap = h.snap();
    assert_eq!(snap.warning, shown_without("the model did not load"));
    assert_eq!(snap.error, "");
}

/// A module's reason as long as Maxine's first-run one, its hint (the
/// part a user acts on) at the end.
fn long_reason() -> String {
    format!(
        "NvVFX_Load(GreenScreen) returned -1209 ({}); see docs/setup/maxine.md, section 3 HINT-END",
        "x".repeat(220)
    )
}

#[test]
fn a_black_failure_keeps_its_whole_reason_in_the_warning_and_the_journal() {
    let reason = long_reason();
    assert!(
        reason.len() > 300 && reason.len() <= 400,
        "{}",
        reason.len()
    );
    // At run time.
    let mut h = H::new("[effect]\nmode = \"blur\"\n", |_| {});
    h.activate(0);
    crate::log::take_lines();
    h.world().effect_status = EffectStatus::Failed(reason.clone());
    h.tick(1000);
    assert_eq!(h.snap().warning, black(&reason));
    let lines = crate::log::take_lines();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("warning: ") && l.contains(&reason)),
        "{lines:?}"
    );
    // At the start, the module's check refusing it.
    let mut h = H::new("[effect]\nmode = \"blur\"\n", |w| {
        w.effect_unavailable = Some(reason.clone())
    });
    crate::log::take_lines();
    h.activate(0);
    assert_eq!(h.snap().warning, black(&reason));
    let lines = crate::log::take_lines();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("warning: ") && l.contains(&reason)),
        "{lines:?}"
    );
}

#[test]
fn a_none_failure_is_in_the_warning_never_in_error() {
    let config = "[effect]\nmode = \"blur\"\non_failure = \"none\"\n";
    let effect_errors = |h: &mut H| {
        errors_published(h)
            .into_iter()
            .filter(|e| e.contains("effect"))
            .collect::<Vec<_>>()
    };
    // Refused at the start, failing at load, failing at run time.
    for case in ["start", "load", "run"] {
        let mut h = H::new(config, |w| match case {
            "start" => w.effect_unavailable = Some("no model".into()),
            "load" => w.effect_fail = Some("no model".into()),
            _ => {}
        });
        h.activate(0);
        if case == "run" {
            h.world().effect_status = EffectStatus::Failed("no model".into());
        }
        h.tick(1000);
        let snap = h.snap();
        assert_eq!(snap.state, State::Active, "{case}");
        assert_eq!(snap.error, "", "{case}");
        assert_eq!(snap.warning, shown_without("no model"), "{case}");
        assert_eq!(effect_errors(&mut h), Vec::<String>::new(), "{case}");
    }
}

#[test]
fn a_failed_effect_whose_module_is_disabled_leaves_one_clause() {
    let config = "[effect]\nmode = \"blur\"\non_failure = \"none\"\n";
    for at_start in [false, true] {
        let mut h = H::new(config, |w| {
            if at_start {
                w.effect_unavailable = Some("no model".into());
            }
        });
        h.activate(0);
        if !at_start {
            h.world().effect_status = EffectStatus::Failed("no model".into());
            h.tick(500);
        }
        assert_eq!(h.snap().warning, shown_without("no model"), "{at_start}");
        h.set_module(MAXINE_MODULE, false, 1000);
        let snap = h.snap();
        assert_eq!(
            (snap.error.as_str(), snap.warning.as_str()),
            ("", NO_BLUR),
            "at start {at_start}: the dropped clause alone"
        );
    }
}

#[test]
fn re_enabling_the_module_gives_a_failed_effect_again() {
    let config = "[effect]\nmode = \"blur\"\non_failure = \"none\"\n";
    for at_start in [false, true] {
        let mut h = H::new(config, |w| {
            if at_start {
                w.effect_unavailable = Some("no model".into());
            }
        });
        h.activate(0);
        if !at_start {
            h.world().effect_status = EffectStatus::Failed("no model".into());
            h.tick(500);
        }
        h.set_module(MAXINE_MODULE, false, 1000);
        // Fixed meanwhile: the module, enabled again, gives it again.
        h.world().effect_unavailable = None;
        h.world().effect_status = EffectStatus::Off;
        h.set_module(MAXINE_MODULE, true, 2000);
        assert_eq!(
            h.last_effect().mode,
            EffectMode::Blur,
            "at start {at_start}"
        );
        h.tick(2500);
        assert_eq!(h.world().effect_status, EffectStatus::Ready);
        let snap = h.snap();
        assert_eq!(
            (snap.error.as_str(), snap.warning.as_str()),
            ("", ""),
            "at start {at_start}"
        );
        assert_eq!(h.world().session_starts, 1, "at start {at_start}");
    }
}

#[test]
fn a_refused_setting_at_start_with_black_names_the_effect_in_error() {
    let mut h = H::new("[effect]\nmode = \"blur\"\n", |w| {
        w.effect_unavailable = Some("the module refuses a strength of 2".into());
        w.fail_start = Some("output pipeline: the effect element did not link".into());
    });
    h.activate_expecting_error();
    assert_eq!(
        h.snap().error,
        "effect blur unavailable: the module refuses a strength of 2"
    );
    // The build's own failure is its error when the effect's check passed.
    let mut h = H::new("[effect]\nmode = \"blur\"\n", |w| {
        w.fail_start = Some("output pipeline: the effect element did not link".into());
    });
    h.activate_expecting_error();
    assert_eq!(
        h.snap().error,
        "output pipeline: the effect element did not link"
    );
}

#[test]
fn an_effect_method_keeps_the_black_warning_until_the_effect_reports() {
    let mut h = H::new("[effect]\nmode = \"blur\"\n", |w| w.effect_slow = true);
    h.activate(0);
    h.world().effect_status = EffectStatus::Failed("no model".into());
    h.tick(500);
    assert_eq!(h.snap().warning, black("no model"));
    h.drain();
    // A retry: the frames stay black while it loads, and so the text.
    h.set_effect("blur", 1000).unwrap();
    assert_eq!(h.snap().warning, black("no model"));
    h.tick(1250);
    assert_eq!(h.world().effect_status, EffectStatus::Loading);
    assert_eq!(h.snap().warning, black("no model"));
    let warnings: Vec<String> = h
        .drain()
        .0
        .into_iter()
        .filter_map(|c| match c {
            Change::Warning(w) => Some(w),
            _ => None,
        })
        .collect();
    assert!(warnings.is_empty(), "never emptied: {warnings:?}");
    // It reports ready: cleared.
    h.world().effect_status = EffectStatus::Ready;
    h.tick(1500);
    assert_eq!(h.snap().warning, "");
    // Under `none` the retry shows black while it loads: the old text
    // goes at once.
    let mut h = H::new("[effect]\nmode = \"blur\"\non_failure = \"none\"\n", |w| {
        w.effect_slow = true
    });
    h.activate(0);
    h.world().effect_status = EffectStatus::Failed("no model".into());
    h.tick(500);
    assert_eq!(h.snap().warning, shown_without("no model"));
    h.set_effect("blur", 1000).unwrap();
    assert_eq!(h.snap().warning, "");
    // And with the effect turned off under `black`, it goes too.
    let mut h = H::new("[effect]\nmode = \"blur\"\n", |w| w.effect_slow = true);
    h.activate(0);
    h.world().effect_status = EffectStatus::Failed("no model".into());
    h.tick(500);
    h.set_effect("none", 1000).unwrap();
    assert_eq!(h.snap().warning, "");
}

#[test]
fn the_warning_is_one_line_whatever_a_module_calls_itself() {
    const EVIL: &str = "org.x.evil";
    let entry = crate::modules::testing::entry(
        &format!(
            "abi = 1\nid = \"{EVIL}\"\nname = \"Evil\\nmodule\\u202E\"\nversion = \"1\"\n\
             licence = \"MIT\"\nkind = \"video-stage\"\nlibrary = \"/m/evil.so\"\n\
             [[stage]]\nid = \"tint\"\nslot = \"post-composite\"\nrank = 1\n\
             device = \"cpu\"\nframes = [\"host\"]\nformats = [\"nv12\"]\n"
        ),
        "/m/evil.toml",
    );
    let settings = crate::modules::testing::settings(
        r#"{"groups":[{"id":"evil","label":"Evil","settings":[
            {"id":"effect.evil.enabled","label":"Warm\ntint\u202e","type":"bool",
             "default":true,"stage":"tint","role":"enable","hint":"switch"}]}]}"#,
        &["tint"],
    );
    let opened = Ok(crate::backend::OpenedModule {
        module: None,
        stages: vec![(
            StageId::new("tint").unwrap(),
            Availability::NotInstalled("no\nmodel\u{202E}".into()),
        )],
        settings,
    });
    let mut h = H::new("", move |w| w.modules = vec![(entry, opened)]);
    h.activate(0);
    let w = h.snap().warning;
    assert_eq!(w, "warm tint unavailable: Evil module: no model");
    assert!(!w.contains(['\n', '\u{202E}']), "{w:?}");
}

#[test]
fn under_black_a_provided_effect_that_fails_is_never_given_off() {
    // While a module of the session provides the effect, `black` never
    // shows the room: every effect the session is given is the effect
    // selected, under `black` (the element's frames black while it fails).
    for case in ["start", "load", "run", "method"] {
        let mut h = H::new("[effect]\nmode = \"blur\"\n", |w| match case {
            "start" => w.effect_unavailable = Some("no model".into()),
            "load" => w.effect_fail = Some("no model".into()),
            _ => {}
        });
        h.activate(0);
        if case == "run" || case == "method" {
            h.world().effect_status = EffectStatus::Failed("no model".into());
        }
        h.tick(500);
        if case == "method" {
            h.world().effect_fail = Some("no model".into());
            h.set_effect("blur", 1000).unwrap();
        }
        h.tick(1500);
        assert_eq!(h.state(), State::Active, "{case}");
        assert_eq!(h.snap().warning, black("no model"), "{case}");
        let w = h.world();
        let started = &w.session.as_ref().unwrap().0.effect;
        for e in std::iter::once(started).chain(w.effect_sets.iter()) {
            assert_eq!(
                (e.mode, e.on_failure),
                (EffectMode::Blur, EffectOnFailure::Black),
                "{case}"
            );
        }
    }
}

#[test]
fn an_effect_whose_module_the_crash_ledger_disabled_is_dropped_not_a_fault() {
    let mut h = H::new("[effect]\nmode = \"blur\"\n", |_| {});
    let ledger = h.paths.state.with_file_name(LEDGER_FILE);
    fs::create_dir_all(ledger.parent().unwrap()).unwrap();
    h.paths.ledger = Some(ledger.clone());
    for _ in 1..=2 {
        let mut l = CrashLedger::open(&ledger);
        l.begin(MAXINE_MODULE).unwrap();
        drop(l);
        h.restart();
    }
    assert!(!h.modules().find(MAXINE_MODULE).unwrap().enabled);
    h.activate(0);
    let snap = h.snap();
    assert_eq!(snap.error, "");
    // The crash ledger's reason, not a generic one.
    assert_eq!(
        snap.warning,
        format!(
            "effect blur unavailable: NVIDIA Maxine video effects: {}",
            obc_module_host::REASON_CRASHED
        )
    );
    assert_eq!(h.applied(), Some(EffectMode::None));
}

/// A module `id` (an NPU, rank 100) whose one stage is `stage` in `slot`,
/// opened with the stage installed.
fn npu_module(
    id: &str,
    stage: StageId,
    slot: &str,
) -> (
    obc_module_host::discovery::ModuleEntry,
    Result<crate::backend::OpenedModule, String>,
) {
    (
        crate::modules::testing::entry(
            &format!(
                "abi = 1\nid = \"{id}\"\nname = \"NPU {stage}\"\nversion = \"1\"\n\
                 licence = \"MIT\"\nkind = \"video-stage\"\nlibrary = \"/m/{id}.so\"\n\
                 [[stage]]\nid = \"{stage}\"\nslot = \"{slot}\"\nrank = 100\n\
                 device = \"npu\"\nframes = [\"host\"]\nformats = [\"nv12\"]\n"
            ),
            &format!("/m/{id}.toml"),
        ),
        Ok(crate::backend::OpenedModule {
            module: None,
            stages: vec![(stage, Availability::Installed)],
            settings: Default::default(),
        }),
    )
}

impl H {
    /// The session's params as it was started.
    fn session_params(&self) -> SessionParams {
        self.world().session.as_ref().expect("a session").0.clone()
    }
}

#[test]
fn an_effect_selected_while_its_module_is_disabled_is_given_once_it_is_enabled_again() {
    let mut h = H::new("", |_| {});
    h.activate(0);
    h.set_module(MAXINE_MODULE, false, 1000);
    h.set_effect("blur", 2000).unwrap();
    assert_eq!(h.applied(), Some(EffectMode::None));
    let snap = h.snap();
    assert_eq!((snap.error.as_str(), snap.warning.as_str()), ("", NO_BLUR));
    assert_eq!(h.state(), State::Active);
    // Its module is enabled again: the session, which has it, runs it
    // with no further call.
    h.set_module(MAXINE_MODULE, true, 3000);
    assert_eq!(h.applied(), Some(EffectMode::Blur));
    assert_eq!(h.last_effect().mode, EffectMode::Blur);
    assert_eq!(h.world().session_starts, 1, "no restart");
    let snap = h.snap();
    assert_eq!((snap.error.as_str(), snap.warning.as_str()), ("", ""));
}

#[test]
fn an_effect_selected_after_its_module_is_enabled_restarts_a_session_started_without_it() {
    let mut h = H::new("", |_| {});
    h.set_module(MAXINE_MODULE, false, 0);
    h.activate(100);
    assert!(h.session_params().backends.effect.is_none());
    // Enabled with nothing of it selected: the session is kept.
    h.set_module(MAXINE_MODULE, true, 1000);
    h.tick(1400);
    assert_eq!(h.world().session_starts, 1);
    // The effect selected: only a new session can run it.
    h.set_effect("blur", 2000).unwrap();
    h.tick(2400);
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.world().session_starts, 2);
    let params = h.session_params();
    assert_eq!(params.effect.mode, EffectMode::Blur);
    assert!(params.backends.effect.is_some());
    assert_eq!(h.applied(), Some(EffectMode::Blur));
    let snap = h.snap();
    assert_eq!((snap.error.as_str(), snap.warning.as_str()), ("", ""));
    // Once: the new session runs it, and is kept.
    h.set_strength(0.3, 3000).unwrap();
    h.tick(5000);
    assert_eq!(h.world().session_starts, 2);
}

#[test]
fn a_stage_of_a_module_enabled_mid_session_comes_back_by_one_restart() {
    let mut h = H::new("[effect]\ndenoise = true\n", |_| {});
    h.set_module(MAXINE_MODULE, false, 0);
    h.activate(100);
    assert!(!stage_on(&h.session_params().effect, "denoise"));
    // A stage other than the background effect is named alone.
    let dropped = "noise removal unavailable: NVIDIA Maxine video effects: Disabled";
    let snap = h.snap();
    assert_eq!((snap.error.as_str(), snap.warning.as_str()), ("", dropped));
    // Idle, the same.
    h.consumers(0, 200);
    h.tick(60_000);
    assert_eq!(h.state(), State::Idle);
    assert_eq!(h.snap().warning, dropped);
    h.activate(60_100);
    assert_eq!(h.world().session_starts, 2);
    h.set_module(MAXINE_MODULE, true, 61_000);
    h.tick(61_400);
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.world().session_starts, 3);
    let params = h.session_params();
    assert!(params.backends.effect.is_some());
    assert!(stage_on(&params.effect, "denoise"), "{params:?}");
    assert_eq!(h.snap().warning, "");
    // Not restarted again.
    h.tick(63_000);
    h.set_module(MAXINE_MODULE, true, 64_000);
    h.tick(66_000);
    assert_eq!(h.world().session_starts, 3);
}

#[test]
fn disabling_the_enhancer_mid_session_reopens_the_camera_without_enhancement() {
    let mut h = H::new("", |w| w.cameras = vec![small(alpha())]);
    h.set_setting(ENHANCE_ID, SettingValue::Bool(true), 0)
        .unwrap();
    h.activate(100);
    assert_eq!(h.enhance_requests(), [true]);
    h.set_module(MAXINE_MODULE, false, 1000);
    assert_eq!(h.enhance_requests(), [true, false]);
    h.set_module(MAXINE_MODULE, true, 2000);
    assert_eq!(h.enhance_requests(), [true, false, true]);
    assert_eq!(
        h.world().session_starts,
        1,
        "the camera reopened, no restart"
    );
    assert_eq!(h.state(), State::Active);
}

#[test]
fn a_pin_to_another_module_keeps_the_running_stage_until_the_next_start() {
    const DN: &str = "org.example.denoise";
    const CUT: &str = "org.example.cutout";
    let mut h = H::new("[effect]\nmode = \"blur\"\ndenoise = true\n", |w| {
        w.modules = vec![
            npu_module(DN, StageId::DENOISE, "pre-matte"),
            npu_module(CUT, StageId::BACKGROUND, "matte"),
        ];
    });
    h.activate(0);
    let started = h.session_params();
    assert!(stage_on(&started.effect, "denoise"));
    let sets = h.world().effect_sets.len();
    for (stage, module) in [("denoise", DN), ("background", CUT)] {
        h.call(
            |r| Command::SetStagePin(stage.into(), module.into(), r),
            1000,
        )
        .unwrap();
    }
    h.tick(1400);
    {
        let w = h.world();
        assert_eq!(w.session_starts, 1, "no restart");
        assert_eq!(w.effect_sets.len(), sets, "nothing given to the session");
    }
    assert_eq!(h.applied(), Some(EffectMode::Blur));
    assert_eq!(h.snap().warning, "");
    // The next start takes the pins.
    h.consumers(0, 2000);
    h.tick(70_000);
    h.activate(71_000);
    let next = h.session_params();
    assert_eq!(
        next.backends.effect.as_ref().map(|m| m.id.as_str()),
        Some(CUT)
    );
    assert!(next.backends.chains.iter().any(|(_, m)| m.id == DN));
}

#[test]
fn an_effect_its_module_can_no_longer_run_moves_to_the_module_that_can() {
    const CUT: &str = "org.example.cutout";
    let mut h = H::new("[effect]\nmode = \"blur\"\n", |w| {
        w.modules = vec![npu_module(CUT, StageId::BACKGROUND, "matte")];
    });
    h.activate(0);
    let id = |h: &H| {
        h.session_params()
            .backends
            .effect
            .as_ref()
            .map(|m| m.id.clone())
    };
    assert_eq!(id(&h).as_deref(), Some(MAXINE_MODULE));
    // Maxine's cut-out cannot run any more; the other module's can.
    h.world().features = Some(stage_missing(&StageId::BACKGROUND, "no model"));
    h.reload("[effect]\nmode = \"blur\"\n", 1000).unwrap();
    h.tick(1400);
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.world().session_starts, 2);
    assert_eq!(id(&h).as_deref(), Some(CUT));
    assert_eq!(h.applied(), Some(EffectMode::Blur));
    let snap = h.snap();
    assert_eq!((snap.error.as_str(), snap.warning.as_str()), ("", ""));
}

#[test]
fn no_warning_is_published_before_the_modules_are_loaded() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        config: dir.path().join("config.toml"),
        state: dir.path().join("state").join("state.toml"),
        cache: None,
        runtime: None,
        ledger: None,
    };
    // A selected effect, which only a module provides.
    fs::write(&paths.config, "[effect]\nmode = \"blur\"\n").unwrap();
    let fake = FakeBackend::default();
    fake.world().cameras = vec![alpha()];
    let (published, _notes) = Published::new(preliminary_snapshot(&paths));
    let mut ctrl =
        Controller::new_without_modules(fake.clone(), paths, published, None, Instant::now());
    let snap = ctrl.snapshot();
    assert_eq!(snap.effect, "blur");
    assert_eq!(
        snap.warning, "",
        "what the modules provide is not known yet"
    );
    ctrl.load_modules(Instant::now());
    assert_eq!(ctrl.snapshot().warning, "", "the effect module provides it");
}

// ---- a session start that learns another provider ----

#[test]
fn a_session_start_that_learns_another_provider_starts_again_in_the_same_step() {
    const CUT: &str = "org.example.cutout";
    let mut h = H::new("[effect]\nmode = \"blur\"\n", |w| {
        w.modules = vec![npu_module(CUT, StageId::BACKGROUND, "matte")];
        w.features = Some(all_installed());
        // Learnt once the GPU is up: Maxine has no cut-out model for it,
        // and the other module takes the background effect over.
        w.features_with_cc = Some(stage_missing(&StageId::BACKGROUND, "no model for sm_86"));
        w.compute_capability = Some((8, 6));
    });
    h.drain();
    h.consumers(1, 0);
    // Driven as `run` drives it: each tick at the wakeup it asked for.
    let mut now = h.at(0);
    let end = h.at(2000);
    while h.state() != State::Active {
        let next = now + h.ctrl.next_wakeup(now);
        if next > end {
            break;
        }
        now = next;
        h.ctrl.tick(now);
    }
    assert_eq!(h.state(), State::Active, "active within 2 s");
    {
        let w = h.world();
        assert_eq!(w.session_starts, 2, "started again once, at once");
        let (params, _) = w.session.as_ref().unwrap();
        assert_eq!(
            params.backends.effect.as_ref().map(|m| m.id.as_str()),
            Some(CUT)
        );
        assert_eq!(params.effect.mode, EffectMode::Blur);
    }
    assert_eq!(h.applied(), Some(EffectMode::Blur));
    let snap = h.snap();
    assert_eq!((snap.error.as_str(), snap.warning.as_str()), ("", ""));
    let changes = h.drain().0;
    assert!(
        !states_in(&changes).contains(&State::Idle),
        "starting is not followed by idle: {changes:?}"
    );
    // And kept: no further restart.
    let from = now.saturating_duration_since(h.t0).as_millis() as u64;
    ticks_over(&mut h, from, Duration::from_secs(10));
    assert_eq!(h.world().session_starts, 2);
    assert_eq!(h.state(), State::Active);
}

#[test]
fn a_provider_lost_again_on_the_second_start_pass_is_named_in_the_warning() {
    const CUT: &str = "org.example.cutout";
    let mut h = H::new("[effect]\nmode = \"blur\"\n", |w| {
        w.modules = vec![npu_module(CUT, StageId::BACKGROUND, "matte")];
        w.features = Some(all_installed());
        // Learnt once the GPU is up: Maxine has no cut-out model for it,
        // and the other module takes the background effect over ...
        w.features_with_cc = Some(stage_missing(&StageId::BACKGROUND, "no model for sm_86"));
        w.compute_capability = Some((8, 6));
        // ... which, probed again on the second pass, has lost it, while
        // Maxine has it again: only a new session could run it.
        w.probe_script = vec![
            (
                CUT.into(),
                vec![(
                    StageId::BACKGROUND,
                    Availability::NotInstalled("the NPU is gone".into()),
                )],
            ),
            (MAXINE_MODULE.into(), all_installed()),
        ];
    });
    h.activate(0);
    assert_eq!(h.world().session_starts, 2, "never a third start");
    assert!(h.world().probe_script.is_empty(), "probed as scripted");
    // The session's module cannot run it, and no restart: the camera
    // image, never silently.
    assert_eq!(h.applied(), Some(EffectMode::None));
    let snap = h.snap();
    assert_eq!(snap.error, "");
    assert_eq!(
        snap.warning,
        "effect blur unavailable: it runs when the camera next starts"
    );
}

#[test]
fn a_session_start_with_one_module_that_loses_a_stage_starts_once() {
    // Maxine alone, no cached capability: what it loses for this GPU is
    // followed in the session, never by a restart.
    let mut h = H::new("[effect]\nmode = \"blur\"\ndenoise = true\n", |w| {
        w.features = Some(all_installed());
        w.features_with_cc = Some(denoise_missing("no Denoise model for this GPU (sm_86)"));
        w.compute_capability = Some((8, 6));
    });
    h.activate(0);
    assert_eq!(h.world().session_starts, 1);
    assert_eq!(h.denoise_applied(), Some(false));
    assert_eq!(
        h.snap().warning,
        format!("noise removal unavailable: NVIDIA Maxine video effects: {REASON_NOT_INSTALLED}")
    );
}

// ---- every selected stage no module provides is in `Warning` ----

/// The example module's id and the warning of its stage, disabled.
const HELLO: &str = "org.example.hello";
const NO_HELLO: &str = "warm tint unavailable: Hello tint: Disabled";

#[test]
fn every_dropped_stage_is_named_in_the_warning_with_its_modules_reason() {
    let config = "[effect]\nmode = \"blur\"\ndenoise = true\nrelight = true\n";
    let mut h = H::new(config, |_| {});
    h.activate(0);
    assert_eq!(h.snap().warning, "");
    h.set_module(MAXINE_MODULE, false, 1000);
    let want =
        "effect blur, noise removal, relighting unavailable: NVIDIA Maxine video effects: Disabled";
    let snap = h.snap();
    assert_eq!((snap.error.as_str(), snap.warning.as_str()), ("", want));
    assert_eq!(h.state(), State::Active);
    let last = h.last_effect();
    assert_eq!(last.mode, EffectMode::None);
    assert!(!stage_on(&last, "denoise") && !stage_on(&last, "relight"));
    // Re-enabled: all three back, the warning cleared.
    h.set_module(MAXINE_MODULE, true, 2000);
    let last = h.last_effect();
    assert_eq!(last.mode, EffectMode::Blur);
    assert!(stage_on(&last, "denoise") && stage_on(&last, "relight"));
    assert_eq!(h.snap().warning, "");
    assert_eq!(h.world().session_starts, 1);
}

#[test]
fn the_warning_gives_why_a_module_failed_to_load() {
    let mut cut = npu_module("org.example.cutout", StageId::BACKGROUND, "matte");
    cut.1 = Err("cannot open /m/org.example.cutout.so: no such file".into());
    let mut h = H::new("[effect]\nmode = \"blur\"\n", |w| {
        w.maxine = false;
        w.modules = vec![cut];
    });
    assert_eq!(
        h.snap().warning,
        "effect blur unavailable: NPU background: cannot open /m/org.example.cutout.so: no \
         such file"
    );
    h.activate(0);
    assert_eq!(h.applied(), Some(EffectMode::None));
    assert_eq!(h.snap().error, "");
}

#[test]
fn the_warning_gives_why_a_stage_is_not_installed() {
    let mut h = H::new("[effect]\ndenoise = true\n", |w| {
        w.features = Some(denoise_missing("no Denoise model"));
    });
    let want =
        format!("noise removal unavailable: NVIDIA Maxine video effects: {REASON_NOT_INSTALLED}");
    assert_eq!(h.snap().warning, want, "idle");
    h.activate(0);
    let snap = h.snap();
    assert_eq!((snap.error.as_str(), snap.warning.as_str()), ("", &*want));
    // Switched off: nothing to say.
    h.set_denoise(false, 1000).unwrap();
    assert_eq!(h.snap().warning, "");
}

#[test]
fn dropped_stages_of_two_modules_are_two_clauses() {
    let dir = hello_modules_dir();
    let config = format!("{}[effect]\nmode = \"blur\"\n", modules_config(dir.path()));
    let mut h = H::new(&config, |w| w.real_modules = true);
    h.set_setting("effect.hello.enabled", SettingValue::Bool(true), 0)
        .unwrap();
    h.activate(100);
    h.set_module(MAXINE_MODULE, false, 1000);
    h.set_module(HELLO, false, 1100);
    let snap = h.snap();
    assert_eq!(
        (snap.error.as_str(), snap.warning.as_str()),
        ("", &*format!("{NO_BLUR}; {NO_HELLO}"))
    );
    assert_eq!(h.state(), State::Active);
    assert!(!stage_on(&h.last_effect(), "hello"));
}

#[test]
fn a_chain_module_toggled_mid_session_drops_and_restores_its_stage() {
    let dir = hello_modules_dir();
    let mut h = H::new(&modules_config(dir.path()), |w| w.real_modules = true);
    h.set_setting("effect.hello.enabled", SettingValue::Bool(true), 0)
        .unwrap();
    h.activate(100);
    assert!(stage_on(&h.session_params().effect, "hello"));
    let ingests = h.world().ingests.len();
    // Disabled: off in the running session, named in the warning.
    h.set_module(HELLO, false, 1000);
    assert!(!stage_on(&h.last_effect(), "hello"));
    let snap = h.snap();
    assert_eq!((snap.error.as_str(), snap.warning.as_str()), ("", NO_HELLO));
    assert_eq!(h.state(), State::Active);
    // Enabled again: back in the same session, the camera kept open.
    h.set_module(HELLO, true, 2000);
    assert!(stage_on(&h.last_effect(), "hello"));
    assert_eq!(h.snap().warning, "");
    {
        let w = h.world();
        assert_eq!(w.session_starts, 1);
        assert_eq!(w.ingests.len(), ingests);
    }
    // A session started with it disabled: enabling it restarts it once.
    h.set_module(HELLO, false, 3000);
    h.consumers(0, 3100);
    h.tick(70_000);
    assert_eq!(h.state(), State::Idle);
    assert_eq!(h.snap().warning, NO_HELLO, "remembered while idle");
    h.activate(71_000);
    assert_eq!(h.world().session_starts, 2);
    assert!(h.session_params().backends.chains.is_empty());
    assert_eq!(h.snap().warning, NO_HELLO, "and in the next session");
    h.set_module(HELLO, true, 72_000);
    h.tick(72_400);
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.world().session_starts, 3);
    assert!(stage_on(&h.session_params().effect, "hello"));
    assert_eq!(h.snap().warning, "");
}

#[test]
fn a_stage_switched_off_is_never_in_the_warning() {
    let dir = hello_modules_dir();
    let mut h = H::new(&modules_config(dir.path()), |w| w.real_modules = true);
    // Noise removal, relighting and the module's stage are off; no effect.
    h.activate(100);
    h.set_module(MAXINE_MODULE, false, 1000);
    h.set_module(HELLO, false, 1100);
    assert_eq!(h.snap().warning, "");
    h.consumers(0, 1200);
    h.tick(70_000);
    assert_eq!(h.snap().warning, "");
}

#[test]
fn enhancement_is_warned_only_where_it_would_apply() {
    let want = "enhance low-resolution camera unavailable: NVIDIA Maxine video effects: Disabled";
    // A camera small enough for it.
    let mut h = H::new("", |w| w.cameras = vec![small(alpha())]);
    h.set_setting(ENHANCE_ID, SettingValue::Bool(true), 0)
        .unwrap();
    h.set_module(MAXINE_MODULE, false, 100);
    assert_eq!(h.snap().warning, want);
    h.activate(200);
    assert_eq!(h.snap().warning, want);
    // A camera at full resolution: nothing to enhance, nothing missing.
    let mut h = H::new("[effect]\nenhance = true\n", |_| {});
    h.set_module(MAXINE_MODULE, false, 100);
    assert_eq!(h.snap().warning, "");
    h.activate(200);
    assert_eq!(h.snap().warning, "");
}

// ---- capture and output modes, frame generation, the Warning bound ----

const BRIO_FORMATS: &str =
    include_str!("../../../obc-pipeline/tests/data/mx-brio-046d-0944.v4l2-ctl.txt");
const FHD_FORMATS: &str =
    include_str!("../../../obc-pipeline/tests/data/fhd-camera-5986-1193-colour.v4l2-ctl.txt");

fn vmode(s: &str) -> obc_core::VideoMode {
    s.parse().unwrap()
}

/// `alpha()` with the MX Brio's modes.
fn brio_cam() -> Camera {
    let mut c = alpha();
    c.modes = obc_pipeline::capability::parse_v4l2_ctl(BRIO_FORMATS).modes;
    c
}

/// `beta()` with the FHD Camera's modes.
fn fhd_cam() -> Camera {
    let mut c = beta();
    c.modes = obc_pipeline::capability::parse_v4l2_ctl(FHD_FORMATS).modes;
    c
}

fn enum_of(s: &Setting) -> (Vec<(String, String)>, String, String) {
    match &s.kind {
        SettingKind::Enum {
            options,
            default,
            value,
        } => (options.clone(), default.clone(), value.clone()),
        other => panic!("not an enum: {other:?}"),
    }
}

fn str_value(v: &str) -> SettingValue {
    SettingValue::Str(v.into())
}

#[test]
fn the_capture_mode_row_offers_the_camera_modes_and_applies_live() {
    let mut h = H::new("", |w| w.cameras = vec![brio_cam(), fhd_cam()]);
    let tree = h.get_settings("", 0).unwrap();
    assert_eq!(tree.groups[0].id, "capture");
    assert_eq!(tree.groups[0].label, "Capture");
    assert_eq!(tree.groups[0].settings[0].id, settings::MODE);
    let s = h.setting(settings::MODE);
    assert_eq!(s.label, "Resolution and frame rate");
    assert_eq!(s.scope, obc_ipc::settings::SCOPE_CAMERA);
    let (options, default, value) = enum_of(&s);
    assert_eq!(options[0], ("auto".into(), "Automatic".into()));
    assert!(options.contains(&("3840x2160@30".into(), "3840x2160 \u{b7} 30 fps".into())));
    assert!(options.len() <= settings::MAX_OPTIONS);
    assert_eq!(
        (default.as_str(), value.as_str(), s.stored),
        ("auto", "auto", false)
    );
    // The FHD Camera has no 4K mode.
    let fhd = h.get_settings("usb-b", 0).unwrap();
    let (options, ..) = enum_of(fhd.find(settings::MODE).unwrap());
    assert_eq!(options[1].0, "1920x1080@30");
    assert!(!options.iter().any(|(v, _)| v.starts_with("3840x2160")));

    // Automatic: the config's 1920x1080@60, NV12 on the Brio.
    h.activate(100);
    assert_eq!(h.world().ingest_modes, [vmode("NV12:1920x1080@60")]);
    let writers = h.world().writer_starts;
    // 4K30: asked without a format, MJPEG chosen; the camera is reopened,
    // the session and the loopback kept.
    h.set_setting(settings::MODE, str_value("3840x2160@30"), 1000)
        .unwrap();
    {
        let w = h.world();
        assert_eq!(
            w.ingest_modes,
            [vmode("NV12:1920x1080@60"), vmode("MJPEG:3840x2160@30")]
        );
        assert_eq!(w.ingests, ["usb-a", "usb-a"]);
        assert_eq!(w.writer_starts, writers, "the loopback is not touched");
        assert_eq!(w.session_starts, 1);
    }
    h.tick(1100);
    assert_eq!(h.state(), State::Active);
    let s = h.setting(settings::MODE);
    assert_eq!((enum_of(&s).2.as_str(), s.stored), ("3840x2160@30", true));
    assert_eq!(
        h.persisted().camera_modes["usb-a"].to_string(),
        "3840x2160@30"
    );
    assert_eq!(h.snap().warning, "");
    // The same mode again: nothing reopened.
    h.set_setting(settings::MODE, str_value("3840x2160@30"), 1200)
        .unwrap();
    assert_eq!(h.world().ingest_modes.len(), 2);
    // Not an option, or not a string: refused, nothing changed.
    assert!(matches!(
        h.set_setting(settings::MODE, str_value("1234x567@30"), 1300),
        Err(IpcError::InvalidArgument(_))
    ));
    assert!(matches!(
        h.set_setting(settings::MODE, SettingValue::Int(30), 1300),
        Err(IpcError::InvalidArgument(_))
    ));
    // Automatic again: the config's mode, the choice forgotten.
    h.set_setting(settings::MODE, str_value("auto"), 2000)
        .unwrap();
    assert_eq!(
        h.world().ingest_modes.last(),
        Some(&vmode("NV12:1920x1080@60"))
    );
    assert!(h.persisted().camera_modes.is_empty());
    // A reset of the group, of the setting or of the camera forgets it.
    for scope in ["group:capture", "setting:camera.capture.mode", "camera"] {
        h.set_setting(settings::MODE, str_value("1280x720@30"), 3000)
            .unwrap();
        assert_eq!(
            h.world().ingest_modes.last(),
            Some(&vmode("NV12:1280x720@30"))
        );
        h.reset_settings(scope, 3100).unwrap();
        assert!(h.persisted().camera_modes.is_empty(), "{scope}");
        assert_eq!(
            h.world().ingest_modes.last(),
            Some(&vmode("NV12:1920x1080@60")),
            "{scope}"
        );
    }
    // Kept across a restart.
    h.set_setting(settings::MODE, str_value("2560x1440@30"), 4000)
        .unwrap();
    h.ctrl.flush_settings();
    h.restart();
    let s = h.setting(settings::MODE);
    assert_eq!(enum_of(&s).2, "2560x1440@30");
}

#[test]
fn a_camera_without_known_modes_has_no_capture_mode_row() {
    let mut h = H::new("", |_| {});
    assert!(
        h.get_settings("", 0)
            .unwrap()
            .find(settings::MODE)
            .is_none()
    );
    assert!(matches!(
        h.set_setting(settings::MODE, str_value("auto"), 0),
        Err(IpcError::UnknownSetting(_))
    ));
}

#[test]
fn a_stored_mode_the_camera_no_longer_offers_takes_the_nearest_with_a_warning() {
    let mut h = H::new("", |w| w.cameras = vec![brio_cam()]);
    h.set_setting(settings::MODE, str_value("3840x2160@30"), 0)
        .unwrap();
    assert_eq!(h.snap().warning, "");
    // The same camera, now reporting the FHD Camera's modes.
    let modes = fhd_cam().modes;
    h.world().cameras[0].modes = modes;
    h.rescan(100);
    assert_eq!(
        h.snap().warning,
        "Alpha Cam does not offer 3840x2160@30: it captures 1920x1080@30 instead"
    );
    // The choice is kept, shown as not offered; the feed streams.
    let s = h.setting(settings::MODE);
    let (options, _, value) = enum_of(&s);
    assert_eq!(value, "3840x2160@30");
    assert_eq!(
        options.last().unwrap(),
        &(
            "3840x2160@30".to_string(),
            "3840x2160 \u{b7} 30 fps (gone)".to_string()
        )
    );
    h.activate(200);
    assert_eq!(h.world().ingest_modes, [vmode("MJPEG:1920x1080@30")]);
    // Automatic clears the warning.
    h.set_setting(settings::MODE, str_value("auto"), 1000)
        .unwrap();
    assert_eq!(h.snap().warning, "");
}

#[test]
fn output_mode_is_set_over_set_config_into_the_managed_file_only() {
    let config = "# mine\n[output]\nmode = \"1920x1080@30\" # keep\n";
    let mut h = H::new(config, |w| w.cameras = vec![brio_cam()]);
    let row = h.row("output.mode");
    assert!(row.editable && row.enabled);
    let obc_ipc::ConfigKind::Enum { options } = &row.kind else {
        panic!("{:?}", row.kind)
    };
    let values: Vec<&str> = options.iter().map(|(v, _)| v.as_str()).collect();
    assert_eq!(values[0], "3840x2160@30");
    assert!(values.contains(&"1280x720@30"));
    // No frame generation: no generated rate.
    assert!(!options.iter().any(|(_, l)| l.ends_with("(generated)")));
    h.set_config(
        "output.mode",
        obc_ipc::ConfigValue::Str("1280x720@30".into()),
    )
    .unwrap();
    assert_eq!(
        h.managed_key("output.mode"),
        Some(toml::Value::String("1280x720@30".into()))
    );
    assert_eq!(fs::read_to_string(&h.paths.config).unwrap(), config);
    // Nobody holds the device: applied at once.
    assert_eq!(h.snap().output_mode, "NV12 1280x720@30");
    let row = h.row("output.mode");
    assert_eq!(row.value, obc_ipc::ConfigValue::Str("1280x720@30".into()));
    // With frame generation offered, the generated rates are options.
    let mut h = H::new("", |w| {
        w.cameras = vec![brio_cam()];
        w.modules = vec![fg_module()];
    });
    let row = h.row("output.mode");
    let obc_ipc::ConfigKind::Enum { options } = &row.kind else {
        panic!("{:?}", row.kind)
    };
    assert_eq!(
        options[0],
        (
            "3840x2160@60".to_string(),
            "3840x2160 \u{b7} 60 fps (generated)".to_string()
        )
    );
}

#[test]
fn an_output_change_waiting_for_consumers_names_them_and_clears_after_the_rebuild() {
    let mut h = H::new("", |w| {
        w.holder_names = ["obs", "obs", "firefox", "chrome", "zoom"]
            .map(String::from)
            .to_vec();
    });
    h.activate(0);
    h.set_config(
        "output.mode",
        obc_ipc::ConfigValue::Str("1280x720@30".into()),
    )
    .unwrap();
    assert_eq!(h.snap().output_mode, "NV12 1920x1080@30", "deferred");
    assert_eq!(
        h.snap().warning,
        "Output 1280x720@30 applies when obs, firefox, chrome and 1 more release the \
         virtual camera"
    );
    let reads = h.world().holder_name_reads;
    h.tick(1000);
    assert_eq!(h.world().holder_name_reads, reads, "read once per count");
    // Unnamed holders.
    h.world().holder_names.clear();
    h.consumers(2, 1100);
    assert_eq!(
        h.snap().warning,
        "Output 1280x720@30 applies when the apps using it release the virtual camera"
    );
    // Released: the writer is rebuilt after the grace, the warning goes.
    h.consumers(0, 2000);
    h.tick(7000);
    assert_eq!(h.snap().output_mode, "NV12 1280x720@30");
    assert_eq!(h.snap().warning, "");
    // A loopback change alone is no output change.
    let mut h = H::new("", |_| {});
    h.consumers(1, 0);
    h.reload("[loopback]\nlabel = \"Other\"\n", 100).unwrap();
    assert_eq!(h.snap().warning, "");
}

#[test]
fn a_new_output_contract_that_cannot_start_reverts_to_the_last_one_that_did() {
    let new: OutputContract = "1280x720@30".parse().unwrap();
    let mut h = H::new("", |w| w.refused_contracts = vec![(new, false)]);
    let starts = h.world().writer_starts;
    h.set_config(
        "output.mode",
        obc_ipc::ConfigValue::Str("1280x720@30".into()),
    )
    .unwrap();
    {
        let w = h.world();
        assert_eq!(w.writer_starts, starts + 1, "the old contract again");
        assert_eq!(w.writer.as_ref().unwrap().1.width, 1920);
    }
    let snap = h.snap();
    assert_eq!(snap.state, State::Idle);
    assert_eq!(snap.error, "");
    assert_eq!(snap.output_mode, "NV12 1920x1080@30");
    assert_eq!(
        snap.warning,
        "Output 1280x720@30 could not start (loopback /dev/video11: cannot stream NV12 \
         1280x720@30): streaming 1920x1080@30 instead"
    );
    // The configuration is not rewritten, and nothing is retried.
    assert_eq!(
        h.managed_key("output.mode"),
        Some(toml::Value::String("1280x720@30".into()))
    );
    h.tick(5000);
    assert_eq!(h.world().writer_starts, starts + 1);
    // The session streams the contract that works.
    h.activate(6000);
    assert_eq!(h.session_params().contract.width, 1920);
    // Another contract asked for: tried, the warning gone.
    h.consumers(0, 7000);
    h.tick(20_000);
    h.set_config(
        "output.mode",
        obc_ipc::ConfigValue::Str("1920x1080@30".into()),
    )
    .unwrap();
    assert_eq!(h.snap().warning, "");
}

#[test]
fn a_missing_loopback_at_a_contract_change_is_retried_never_a_failed_contract() {
    let mut h = H::new("", |_| {});
    // The new contract and a loopback that cannot be found: not the
    // contract's fault, so it is not pinned to the old one.
    h.world().sink = Err("no loopback device labelled \"Other\"".into());
    h.set_config(
        "output.mode",
        obc_ipc::ConfigValue::Str("1280x720@30".into()),
    )
    .unwrap();
    assert!(h.ctrl.failed_contract.is_none());
    assert_eq!(h.state(), State::Error);
    assert!(
        h.snap().error.contains("no loopback device"),
        "{}",
        h.snap().error
    );
    // Found again: the new contract starts.
    h.world().sink = Ok("/dev/video11".into());
    h.tick(3000);
    assert_eq!(h.world().writer.as_ref().unwrap().1.width, 1280);
    assert_eq!(h.snap().warning, "");
    assert_eq!(h.state(), State::Idle);
}

#[test]
fn a_new_output_contract_on_a_held_loopback_keeps_the_last_writer_and_waits() {
    let new: OutputContract = "1280x720@30".parse().unwrap();
    let mut h = H::new("", |w| w.refused_contracts = vec![(new, true)]);
    let starts = h.world().writer_starts;
    h.set_config(
        "output.mode",
        obc_ipc::ConfigValue::Str("1280x720@30".into()),
    )
    .unwrap();
    // The last good writer streams again: never a dead device, never a
    // failed contract.
    assert_eq!(
        h.world().writer_starts,
        starts + 1,
        "the old contract again"
    );
    assert_eq!(h.world().writer.as_ref().unwrap().1.width, 1920);
    let snap = h.snap();
    assert_eq!(snap.state, State::Idle);
    assert_eq!(snap.error, "");
    assert_eq!(snap.output_mode, "NV12 1920x1080@30");
    let waits =
        format!("Output 1280x720@30 applies when {HOLDERS_UNKNOWN} release the virtual camera");
    assert_eq!(snap.warning, waits);
    assert!(h.ctrl.failed_contract.is_none());
    // Not tried again before the retry is due.
    h.tick(1000);
    assert_eq!(h.world().writer_starts, starts + 1);
    // Still held at the retry: the last writer again, the wait doubled.
    h.tick(2500);
    assert_eq!(h.world().writer_starts, starts + 2);
    assert_eq!(h.world().writer.as_ref().unwrap().1.width, 1920);
    assert_eq!(h.snap().warning, waits);
    h.tick(5000);
    assert_eq!(
        h.world().writer_starts,
        starts + 2,
        "4 s after the last try"
    );
    // Released: the new contract starts, the warning goes.
    h.world().refused_contracts.clear();
    h.tick(7000);
    assert_eq!(h.world().writer.as_ref().unwrap().1.width, 1280);
    assert_eq!(h.snap().output_mode, "NV12 1280x720@30");
    assert_eq!(h.snap().warning, "");
    assert_eq!(h.state(), State::Idle);
}

impl H {
    /// The daemon restarted, as [`H::restart`], with `setup` applied to the
    /// new world before the controller starts.
    fn restart_with(&mut self, setup: impl FnOnce(&mut World)) {
        let fake = FakeBackend::default();
        {
            let old = self.fake.world();
            let mut w = fake.world();
            w.cameras = old.cameras.clone();
            w.controls = old.controls.clone();
            setup(&mut w);
        }
        self.ctrl.shutdown();
        let (ctrl, notes) = Self::controller(&fake, &self.paths, self.t0);
        self.ctrl = ctrl;
        self.notes = notes;
        self.fake = fake;
    }

    /// When the held contract is tried again, in ms from `t0`, and the
    /// wait that led there.
    fn held_retry(&self) -> (u64, Duration) {
        let r = self.ctrl.held_retry.as_ref().expect("a held retry");
        let at = u64::try_from((r.at - self.t0).as_millis()).unwrap();
        (at, r.delay)
    }
}

#[test]
fn a_loopback_change_is_not_delayed_by_a_held_output_retry() {
    let new: OutputContract = "1280x720@30".parse().unwrap();
    let mut h = H::new("", |w| w.refused_contracts = vec![(new, true)]);
    let starts = h.world().writer_starts;
    h.set_config(
        "output.mode",
        obc_ipc::ConfigValue::Str("1280x720@30".into()),
    )
    .unwrap();
    assert_eq!(h.world().writer_starts, starts + 1);
    assert_eq!(h.held_retry().0, 2000);
    // Inside the wait, the loopback changes: the writer restarts on it in
    // that step (the new contract tried there, the last good one again as
    // it is still held), not at the old retry time.
    h.reload("[loopback]\nlabel = \"Other\"\n", 500).unwrap();
    assert_eq!(h.world().writer_starts, starts + 2, "restarted at once");
    let w = h.ctrl.writer.as_ref().unwrap();
    assert_eq!(w.sink, h.ctrl.config.sink_selector());
    assert_eq!(w.contract.width, 1920);
    // The wait is the new device's, from the start.
    let r = h.ctrl.held_retry.as_ref().unwrap();
    assert_eq!(r.sink, h.ctrl.config.sink_selector());
    assert_eq!(h.held_retry(), (2500, WRITER_RETRY));
}

#[test]
fn the_held_retry_starts_again_at_two_seconds_when_the_configuration_comes_back_to_the_contract() {
    let new: OutputContract = "1280x720@30".parse().unwrap();
    let mut h = H::new("", |w| w.refused_contracts = vec![(new, true)]);
    let set = |h: &mut H, mode: &str| {
        h.set_config("output.mode", obc_ipc::ConfigValue::Str(mode.into()))
            .unwrap();
    };
    set(&mut h, "1280x720@30");
    h.tick(2100);
    h.tick(6200);
    assert_eq!(h.held_retry(), (14_200, Duration::from_secs(8)));
    // Back to the contract that streams: the record goes, and the warning.
    set(&mut h, "1920x1080@30");
    assert!(h.ctrl.held_retry.is_none());
    assert_eq!(h.snap().warning, "");
    // The new contract again: tried at once, next try 2 s later, not 16.
    let starts = h.world().writer_starts;
    h.tick(7000);
    set(&mut h, "1280x720@30");
    assert_eq!(h.world().writer_starts, starts + 1, "tried at once");
    let (at, delay) = h.held_retry();
    assert_eq!(delay, WRITER_RETRY);
    h.tick(at - 100);
    assert_eq!(h.world().writer_starts, starts + 1);
    h.tick(at);
    assert_eq!(h.world().writer_starts, starts + 2);
}

#[test]
fn the_held_retry_wait_doubles_up_to_the_cap() {
    let new: OutputContract = "1280x720@30".parse().unwrap();
    let mut h = H::new("", |w| w.refused_contracts = vec![(new, true)]);
    h.set_config(
        "output.mode",
        obc_ipc::ConfigValue::Str("1280x720@30".into()),
    )
    .unwrap();
    for secs in [2, 4, 8, 16, 32, 32, 32] {
        let (at, delay) = h.held_retry();
        assert_eq!(delay, Duration::from_secs(secs));
        assert!(delay <= HELD_RETRY_MAX);
        let starts = h.world().writer_starts;
        h.tick(at - 1);
        assert_eq!(h.world().writer_starts, starts, "not before {at} ms");
        // Still tried at the cap.
        h.tick(at);
        assert_eq!(h.world().writer_starts, starts + 1, "tried at {at} ms");
        assert_eq!(h.world().writer.as_ref().unwrap().1.width, 1920);
    }
}

#[test]
fn a_held_output_whose_last_good_writer_cannot_start_either_is_the_writer_error() {
    let new: OutputContract = "1280x720@30".parse().unwrap();
    let good = OutputContract::default();
    let mut h = H::new("", |w| w.refused_contracts = vec![(new, true)]);
    h.world().refused_contracts.push((good, false));
    h.set_config(
        "output.mode",
        obc_ipc::ConfigValue::Str("1280x720@30".into()),
    )
    .unwrap();
    assert_eq!(h.state(), State::Error);
    assert!(h.world().writer.is_none());
    assert!(
        h.snap().error.contains("cannot stream NV12 1920x1080@30"),
        "{}",
        h.snap().error
    );
    assert!(h.ctrl.failed_contract.is_none());
    // Retried every 2 s.
    h.tick(1000);
    assert_eq!(h.state(), State::Error);
    h.world().refused_contracts.retain(|(c, _)| *c != good);
    h.tick(1900);
    assert!(h.world().writer.is_none(), "not before the retry");
    h.tick(2100);
    assert_eq!(h.world().writer.as_ref().unwrap().1, good);
    assert_eq!(h.state(), State::Idle);
    assert_eq!(h.snap().error, "");
    assert_eq!(
        h.snap().warning,
        format!("Output 1280x720@30 applies when {HOLDERS_UNKNOWN} release the virtual camera")
    );
    assert!(h.ctrl.failed_contract.is_none());
}

#[test]
fn a_start_on_a_loopback_held_in_another_format_streams_that_format_and_waits() {
    // The daemon starts with no writer of this run to fall back to; the
    // device is held in 1920x1080@25.
    let new: OutputContract = "1280x720@30".parse().unwrap();
    let held: OutputContract = "1920x1080@25".parse().unwrap();
    let mut h = H::new("[output]\nmode = \"1280x720@30\"\n", |w| {
        w.refused_contracts = vec![(new, true)];
        w.held_as = Some(held);
    });
    assert_eq!(h.world().writer.as_ref().unwrap().1, held);
    let snap = h.snap();
    assert_eq!(snap.state, State::Idle);
    assert_eq!(snap.error, "");
    assert_eq!(snap.output_mode, "NV12 1920x1080@25");
    assert_eq!(
        snap.warning,
        format!("Output 1280x720@30 applies when {HOLDERS_UNKNOWN} release the virtual camera")
    );
    assert!(h.ctrl.failed_contract.is_none());
    // Released: the configured contract starts at the retry.
    h.world().refused_contracts.clear();
    h.tick(2100);
    assert_eq!(h.world().writer.as_ref().unwrap().1, new);
    assert_eq!(h.snap().warning, "");
}

#[test]
fn a_refused_contract_at_the_first_start_falls_back_to_the_default_contract() {
    let new: OutputContract = "1280x720@30".parse().unwrap();
    let mut h = H::new("[output]\nmode = \"1280x720@30\"\n", |w| {
        w.refused_contracts = vec![(new, false)];
    });
    assert_eq!(
        h.world().writer.as_ref().unwrap().1,
        OutputContract::default()
    );
    let snap = h.snap();
    assert_eq!(snap.state, State::Idle);
    assert_eq!(snap.error, "");
    assert_eq!(
        snap.warning,
        "Output 1280x720@30 could not start (loopback /dev/video11: cannot stream NV12 \
         1280x720@30): streaming 1920x1080@30 instead"
    );
    // The session streams it.
    h.activate(100);
    assert_eq!(h.session_params().contract, OutputContract::default());
}

#[test]
fn a_restart_streams_the_last_good_contract_of_the_earlier_run_while_the_configured_one_is_held() {
    let good: OutputContract = "1280x720@30".parse().unwrap();
    let mut h = H::new("[output]\nmode = \"1280x720@30\"\n", |_| {});
    // A writer and a session output start with it: the last good one,
    // kept for the next run.
    h.activate(0);
    assert_eq!(h.persisted().output_good, Some(good));
    // A new format, deferred while the app holds the virtual camera, then
    // the daemon restarts with the device still held in another format it
    // cannot name.
    let new: OutputContract = "1280x720@25".parse().unwrap();
    h.reload("[output]\nmode = \"1280x720@25\"\n", 1000)
        .unwrap();
    h.restart_with(|w| w.refused_contracts = vec![(new, true)]);
    // The last good contract of the earlier run streams, not the default.
    assert_eq!(h.world().writer.as_ref().unwrap().1, good);
    let snap = h.snap();
    assert_eq!(snap.error, "");
    assert_eq!(snap.output_mode, "NV12 1280x720@30");
    assert_eq!(
        snap.warning,
        format!("Output 1280x720@25 applies when {HOLDERS_UNKNOWN} release the virtual camera")
    );
    // And a session streams it.
    h.activate(2000);
    assert_eq!(h.session_params().contract, good);
}

#[test]
fn a_chosen_capture_mode_that_cannot_open_falls_back_to_the_automatic_mode_with_a_warning() {
    let mut h = H::new("", |w| {
        w.cameras = vec![brio_cam()];
        w.fail_ingest_modes = vec![("usb-a".into(), vmode("MJPEG:3840x2160@30"))];
    });
    h.activate(100);
    h.set_setting(settings::MODE, str_value("3840x2160@30"), 1000)
        .unwrap();
    // Tried, failed before a first frame: the automatic mode at once, the
    // feed kept.
    assert_eq!(
        h.world().ingest_modes,
        [
            vmode("NV12:1920x1080@60"),
            vmode("MJPEG:3840x2160@30"),
            vmode("NV12:1920x1080@60")
        ]
    );
    h.tick(1100);
    assert_eq!(h.state(), State::Active);
    assert_eq!(
        h.snap().warning,
        "Alpha Cam could not open 3840x2160@30 (reopening Alpha Cam (usb-a): Alpha Cam \
         (usb-a): cannot open): it captures 1920x1080@60 instead"
    );
    // The choice is kept, in the row and the state file.
    let s = h.setting(settings::MODE);
    assert_eq!((enum_of(&s).2.as_str(), s.stored), ("3840x2160@30", true));
    assert_eq!(
        h.persisted().camera_modes["usb-a"].to_string(),
        "3840x2160@30"
    );
    // Automatic: the warning goes.
    h.set_setting(settings::MODE, str_value("auto"), 2000)
        .unwrap();
    assert_eq!(h.snap().warning, "");
}

#[test]
fn a_failed_capture_mode_is_tried_again_only_when_chosen_again() {
    let mut h = H::new("", |w| {
        w.cameras = vec![brio_cam()];
        w.fail_ingest_modes = vec![("usb-a".into(), vmode("MJPEG:3840x2160@30"))];
    });
    h.set_setting(settings::MODE, str_value("3840x2160@30"), 0)
        .unwrap();
    // The first start tries it, then the automatic mode.
    h.activate(100);
    assert_eq!(
        h.world().ingest_modes,
        [vmode("MJPEG:3840x2160@30"), vmode("NV12:1920x1080@60")]
    );
    assert!(h.snap().warning.contains("could not open 3840x2160@30"));
    // The next session of this run does not try it again.
    h.consumers(0, 1000);
    h.tick(7000);
    h.activate(8000);
    assert_eq!(
        h.world().ingest_modes.last(),
        Some(&vmode("NV12:1920x1080@60"))
    );
    assert_eq!(h.world().ingest_modes.len(), 3);
    // Chosen again: tried again (and it fails again).
    h.set_setting(settings::MODE, str_value("3840x2160@30"), 9000)
        .unwrap();
    assert_eq!(
        h.world().ingest_modes[3..],
        [vmode("MJPEG:3840x2160@30"), vmode("NV12:1920x1080@60")]
    );
    assert_eq!(h.state(), State::Active);
    // Now it opens: chosen again, it is the mode.
    h.world().fail_ingest_modes.clear();
    h.set_setting(settings::MODE, str_value("3840x2160@30"), 10_000)
        .unwrap();
    assert_eq!(
        h.world().ingest_modes.last(),
        Some(&vmode("MJPEG:3840x2160@30"))
    );
    h.tick(10_100);
    assert_eq!(h.snap().warning, "");
    // The camera reporting other modes forgets the failure too.
    h.world().fail_ingest_modes = vec![("usb-a".into(), vmode("MJPEG:3840x2160@30"))];
    h.consumers(0, 11_000);
    h.tick(17_000);
    h.ctrl.mode_failed.insert(
        "usb-a".into(),
        super::ModeFailed {
            asked: "3840x2160@30".parse().unwrap(),
            why: "x".into(),
            modes: Vec::new(),
            confirmed: true,
        },
    );
    h.rescan(18_000);
    assert!(h.ctrl.mode_failed.is_empty());
}

#[test]
fn a_camera_that_fails_after_its_first_frame_is_not_blamed_on_its_mode() {
    let mut h = H::new("", |w| w.cameras = vec![brio_cam(), fhd_cam()]);
    h.set_setting(settings::MODE, str_value("3840x2160@30"), 0)
        .unwrap();
    h.activate(100);
    assert_eq!(h.world().ingest_modes, [vmode("MJPEG:3840x2160@30")]);
    // Streaming, then the camera fails: the next camera takes over, the
    // mode is not blamed.
    h.world().failure = Some(Failure::Camera("Alpha Cam: device lost".into()));
    h.tick(1000);
    assert!(h.ctrl.mode_failed.is_empty());
    assert_eq!(h.world().ingests.last().map(String::as_str), Some("usb-b"));
    assert!(!h.snap().warning.contains("could not open"));
}

#[test]
fn a_capture_mode_is_not_blamed_when_the_automatic_mode_cannot_open_either() {
    // Chosen while streaming: the chosen mode fails, then the automatic
    // one too (the camera busy): not the mode's fault.
    let mut h = H::new("", |w| w.cameras = vec![brio_cam()]);
    h.activate(100);
    h.world().fail_ingest = vec!["usb-a".into()];
    let _ = h.set_setting(settings::MODE, str_value("3840x2160@30"), 1000);
    assert_eq!(
        h.world().ingest_modes[1..],
        [vmode("MJPEG:3840x2160@30"), vmode("NV12:1920x1080@60")]
    );
    assert!(h.ctrl.mode_failed.is_empty());
    assert_eq!(h.state(), State::Error);
    assert!(
        !h.snap().warning.contains("could not open"),
        "{:?}",
        h.snap()
    );
    // The camera free again: the chosen mode is tried first, and opens.
    h.world().fail_ingest.clear();
    h.consumers(0, 2000);
    h.tick(9000);
    h.activate(10_000);
    assert_eq!(
        h.world().ingest_modes.last(),
        Some(&vmode("MJPEG:3840x2160@30"))
    );
    assert_eq!(h.snap().warning, "");
    // At a session start: the same.
    h.consumers(0, 11_000);
    h.tick(18_000);
    h.world().fail_ingest = vec!["usb-a".into()];
    let opens = h.world().ingest_modes.len();
    h.consumers(1, 19_000);
    h.tick(19_300);
    assert_eq!(
        h.world().ingest_modes[opens..],
        [vmode("MJPEG:3840x2160@30"), vmode("NV12:1920x1080@60")]
    );
    assert!(h.ctrl.mode_failed.is_empty());
    assert_eq!(h.state(), State::Error);
    // The automatic mode opens but delivers no frame either: not blamed
    // (the record goes at the timeout).
    h.consumers(0, 20_000);
    h.tick(28_000);
    h.world().fail_ingest.clear();
    h.world().fail_ingest_modes = vec![("usb-a".into(), vmode("MJPEG:3840x2160@30"))];
    h.world().frames = false;
    h.consumers(1, 29_000);
    h.tick(29_300);
    assert!(!h.ctrl.mode_failed["usb-a"].confirmed);
    assert!(!h.snap().warning.contains("could not open"));
    h.tick(29_300 + START_TIMEOUT.as_millis() as u64 + 100);
    assert!(h.ctrl.mode_failed.is_empty());
    // It delivers in the automatic mode: the mode is blamed.
    h.world().frames = true;
    h.consumers(0, 40_000);
    h.tick(48_000);
    h.consumers(1, 49_000);
    h.tick(49_300);
    assert_eq!(h.state(), State::Active);
    assert!(h.ctrl.mode_failed["usb-a"].confirmed);
    assert!(h.snap().warning.contains("could not open 3840x2160@30"));
}

/// A module that provides frame generation.
fn fg_module() -> (
    obc_module_host::discovery::ModuleEntry,
    Result<crate::backend::OpenedModule, String>,
) {
    npu_module(
        "org.example.fg",
        StageId::new(FRAME_GENERATION).unwrap(),
        "post-composite",
    )
}

/// The chains of the session as started: (name, stage ids and on).
fn chains_of(h: &H) -> Vec<(String, Vec<(String, bool)>)> {
    h.session_params()
        .effect
        .chains
        .iter()
        .map(|c| {
            (
                c.name.clone(),
                c.stages
                    .iter()
                    .map(|s| (s.id.to_string(), s.enabled))
                    .collect(),
            )
        })
        .collect()
}

#[test]
fn frame_generation_runs_last_when_the_output_rate_is_twice_the_camera_rate() {
    // A 30 fps camera (the fake's NV12 1920x1080@30) into 60.
    let mut h = H::new("[output]\nmode = \"1920x1080@60\"\n", |w| {
        w.modules = vec![
            npu_module(
                "org.example.hello",
                StageId::new("hello").unwrap(),
                "post-composite",
            ),
            fg_module(),
        ];
    });
    assert!(!h.ctrl.order_ids().contains(&FRAME_GENERATION.to_string()));
    h.activate(0);
    let chains = chains_of(&h);
    assert_eq!(
        chains.last().unwrap(),
        &(
            "org.example.fg-gen".to_string(),
            vec![(FRAME_GENERATION.to_string(), true)]
        ),
        "{chains:?}"
    );
    assert_eq!(h.snap().warning, "");
    // Never a toggle of its own.
    let tree = h.get_settings("", 0).unwrap();
    assert!(
        tree.groups
            .iter()
            .flat_map(|g| g.settings.iter())
            .all(|s| !s.id.contains("frame"))
    );
    // Dropped at runtime: the frames repeat, and Warning says so.
    h.set_module("org.example.fg", false, 1000);
    assert_eq!(
        h.snap().warning,
        "60 fps output repeats frames: no frame generation available"
    );
    assert_eq!(h.state(), State::Active);
}

#[test]
fn frame_generation_is_left_out_unless_the_rate_doubles() {
    // Same rates: no frame generation.
    let mut h = H::new("[output]\nmode = \"1920x1080@30\"\n", |w| {
        w.modules = vec![fg_module()]
    });
    h.activate(0);
    assert!(
        chains_of(&h)
            .iter()
            .all(|(_, stages)| stages.iter().all(|(id, _)| id != FRAME_GENERATION))
    );
    assert_eq!(h.snap().warning, "");
    // A 60 fps camera into 60.
    let mut h = H::new("[output]\nmode = \"1920x1080@60\"\n", |w| {
        w.modules = vec![fg_module()];
        w.cameras = vec![brio_cam()];
    });
    h.activate(0);
    assert_eq!(h.world().ingest_modes, [vmode("NV12:1920x1080@60")]);
    // Planned, as the camera offers 30 fps modes, but off.
    assert!(
        chains_of(&h)
            .iter()
            .all(|(_, stages)| stages.iter().all(|(id, on)| id != FRAME_GENERATION || !on))
    );
    // A generated rate with no module to make it: repeats, with a warning.
    let mut h = H::new("[output]\nmode = \"1920x1080@60\"\n", |_| {});
    let want = "60 fps output repeats frames: no frame generation available";
    assert_eq!(h.snap().warning, want);
    h.activate(0);
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.snap().warning, want);
    assert!(
        chains_of(&h)
            .iter()
            .all(|(_, stages)| stages.iter().all(|(id, _)| id != FRAME_GENERATION))
    );
}

#[test]
fn frame_generation_follows_a_capture_mode_change() {
    let mut h = H::new("[output]\nmode = \"1920x1080@60\"\n", |w| {
        w.modules = vec![fg_module()];
        w.cameras = vec![brio_cam()];
    });
    h.set_setting(settings::MODE, str_value("1920x1080@30"), 0)
        .unwrap();
    h.activate(100);
    assert_eq!(
        chains_of(&h).last().unwrap().1,
        [(FRAME_GENERATION.to_string(), true)]
    );
    // 60 fps from the camera: the stage is turned off in the session.
    h.set_setting(settings::MODE, str_value("1920x1080@60"), 1000)
        .unwrap();
    let last = h.world().effect_sets.last().cloned().expect("given again");
    assert!(!stage_on(&last, FRAME_GENERATION));
    assert_eq!(h.snap().warning, "");
    assert_eq!(h.state(), State::Active);
}

#[test]
fn frame_generation_starts_mid_session_when_the_camera_rate_halves() {
    // A session that starts at 60 from the camera into 60 has the stage
    // planned but off: the Brio offers 30 fps modes.
    let mut h = H::new("[output]\nmode = \"1920x1080@60\"\n", |w| {
        w.modules = vec![fg_module()];
        w.cameras = vec![brio_cam()];
    });
    h.activate(0);
    assert_eq!(h.world().ingest_modes, [vmode("NV12:1920x1080@60")]);
    assert_eq!(
        chains_of(&h).last().unwrap(),
        &(
            "org.example.fg-gen".to_string(),
            vec![(FRAME_GENERATION.to_string(), false)]
        )
    );
    let starts = h.world().session_starts;
    // 30 fps from the camera: the stage is turned on in the same session.
    h.set_setting(settings::MODE, str_value("1920x1080@30"), 1000)
        .unwrap();
    let last = h.world().effect_sets.last().cloned().expect("given again");
    assert!(stage_on(&last, FRAME_GENERATION));
    assert_eq!(h.world().session_starts, starts, "no restart");
    assert_eq!(
        h.world().ingest_modes.last(),
        Some(&vmode("NV12:1920x1080@30"))
    );
    assert_eq!(h.snap().warning, "");
    assert_eq!(h.state(), State::Active);
    // And off again at 60.
    h.set_setting(settings::MODE, str_value("1920x1080@60"), 2000)
        .unwrap();
    let last = h.world().effect_sets.last().cloned().expect("given again");
    assert!(!stage_on(&last, FRAME_GENERATION));
    assert_eq!(h.snap().warning, "");
}

#[test]
fn frame_generation_follows_a_fail_over_to_a_camera_of_half_the_rate() {
    // The Brio at 60 into 60, then it fails: the FHD Camera takes over at
    // 30 and frame generation makes the 60.
    let mut h = H::new("[output]\nmode = \"1920x1080@60\"\n", |w| {
        w.modules = vec![fg_module()];
        w.cameras = vec![brio_cam(), fhd_cam()];
    });
    h.activate(0);
    assert!(!chains_of(&h).last().unwrap().1[0].1);
    let starts = h.world().session_starts;
    h.world().failure = Some(Failure::Camera("Alpha: streaming error".into()));
    h.tick(1000);
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.snap().active_camera, "usb-b");
    assert!((h.world().ingest_modes.last().unwrap().fps() - 30.0).abs() < 0.01);
    let last = h.world().effect_sets.last().cloned().expect("given again");
    assert!(stage_on(&last, FRAME_GENERATION));
    assert_eq!(h.world().session_starts, starts, "no restart");
    assert_eq!(h.snap().warning, "");
}

#[test]
fn the_warning_is_bounded_on_a_character_boundary() {
    let mut h = H::new("", |_| {});
    let reason = "é".repeat(400);
    h.ctrl.effect_failure = Some(format!("effect blur unavailable: {reason}"));
    let w = h.ctrl.warning();
    assert!(w.len() <= WARNING_MAX, "{}", w.len());
    assert!(w.ends_with('\u{2026}'), "{w}");
    assert!(w.starts_with("effect blur unavailable: é"));
    assert!(w.len() > WARNING_MAX - 8, "{}", w.len());
    // Short text is as it was.
    assert_eq!(cap_warning("a  b"), "a b");
    let exact = "x".repeat(WARNING_MAX);
    assert_eq!(cap_warning(&exact), exact);
    let cut = cap_warning(&format!("{exact}y"));
    assert_eq!(cut.len(), WARNING_MAX);
    assert!(cut.ends_with('\u{2026}'));
}

#[test]
fn holders_are_named_without_duplicates_up_to_three() {
    let t = |n: &[&str]| holders_text(&n.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    assert_eq!(t(&[]), HOLDERS_UNKNOWN);
    assert_eq!(t(&["obs"]), "obs");
    assert_eq!(t(&["obs", "obs", "zoom"]), "obs and zoom");
    assert_eq!(t(&["a", "b", "c"]), "a, b and c");
    assert_eq!(t(&["a", "b", "c", "d", "e", "a"]), "a, b, c and 2 more");
    assert_eq!(t(&["\u{202e}evil\nname"]), "evil name");
}

#[test]
fn a_gpu_fault_at_a_session_start_never_fails_the_new_contract() {
    let mut h = H::new("", |_| {});
    h.activate(0);
    h.consumers(0, 1000);
    h.tick(10_000);
    h.set_config(
        "output.mode",
        obc_ipc::ConfigValue::Str("1280x720@30".into()),
    )
    .unwrap();
    // Not the contract's fault: the error path, the contract kept.
    h.world().fail_start = Some("CUDA initialisation failed".into());
    h.consumers(1, 11_000);
    h.tick(11_300);
    assert_eq!(h.state(), State::Error);
    assert!(h.ctrl.failed_contract.is_none());
    assert_eq!(h.snap().warning, "");
    assert_eq!(h.snap().output_mode, "NV12 1280x720@30");
    // The GPU back: the next session streams the new contract.
    h.world().fail_start = None;
    h.consumers(0, 12_000);
    h.tick(20_000);
    h.activate(21_000);
    assert_eq!(h.session_params().contract.width, 1280);
}

#[test]
fn a_session_output_that_cannot_start_even_without_its_effects_reverts_once_the_camera_is_released()
{
    let mut h = H::new("", |_| {});
    h.activate(0);
    h.consumers(0, 1000);
    h.tick(10_000);
    h.set_config(
        "output.mode",
        obc_ipc::ConfigValue::Str("1280x720@30".into()),
    )
    .unwrap();
    // The writer takes it; the session output cannot start with it, with
    // or without the effects.
    assert_eq!(h.world().writer.as_ref().unwrap().1.width, 1280);
    h.world().fail_start = Some("cudaconvertscale refused 1280x720".into());
    h.world().fail_start_contract = true;
    h.consumers(1, 11_000);
    h.tick(11_300);
    assert_eq!(h.state(), State::Error);
    assert!(h.world().session.is_none());
    // The app keeps the format it opened: no revert under it.
    assert_eq!(h.world().writer.as_ref().unwrap().1.width, 1280);
    assert_eq!(
        h.snap().warning,
        format!(
            "Output 1280x720@30 could not start (cudaconvertscale refused 1280x720): streaming \
             1920x1080@30 instead; Output 1920x1080@30 applies when {HOLDERS_UNKNOWN} release \
             the virtual camera"
        )
    );
    // Released: the writer reverts to the last good contract, and the next
    // app gets the camera at it.
    h.world().fail_start = None;
    h.world().fail_start_contract = false;
    h.consumers(0, 12_000);
    h.tick(20_000);
    assert_eq!(h.world().writer.as_ref().unwrap().1.width, 1920);
    assert_eq!(h.snap().output_mode, "NV12 1920x1080@30");
    h.activate(21_000);
    assert_eq!(h.session_params().contract.width, 1920);
    // The configured one is kept for when it is picked again.
    assert_eq!(
        h.managed_key("output.mode"),
        Some(toml::Value::String("1280x720@30".into()))
    );
}

#[test]
fn the_last_good_contract_is_kept_only_once_a_frame_was_streamed() {
    let mut h = H::new("[output]\nmode = \"1280x720@30\"\n", |w| w.frames = false);
    let contract: OutputContract = "1280x720@30".parse().unwrap();
    // The output started, but no frame reached it: not proven.
    h.consumers(1, 0);
    h.tick(300);
    assert!(h.world().session.is_some());
    assert_eq!(h.state(), State::Starting);
    assert_eq!(h.ctrl.session_good, None);
    assert_eq!(h.persisted().output_good, None);
    // A frame: the last good contract, kept for the next run.
    h.world().frames = true;
    h.tick(400);
    assert_eq!(h.state(), State::Active);
    assert_eq!(h.ctrl.session_good, Some(contract));
    assert_eq!(h.persisted().output_good, Some(contract));
}

#[test]
fn a_session_output_that_cannot_start_at_a_new_contract_streams_without_its_effects() {
    let mut h = H::new("", |_| {});
    h.activate(0);
    h.consumers(0, 1000);
    h.tick(10_000);
    h.set_config(
        "output.mode",
        obc_ipc::ConfigValue::Str("1280x720@30".into()),
    )
    .unwrap();
    assert_eq!(h.snap().output_mode, "NV12 1280x720@30");
    // The start fails with the effects: the session starts once more
    // without them, at the same contract.
    h.world().fail_start_effects = Some("obc-module: no CUDA memory".into());
    h.world().fail_start_contract = true;
    h.consumers(1, 11_000);
    h.tick(11_300);
    assert_eq!(h.state(), State::Active, "{:?}", h.snap());
    let params = h.session_params();
    assert_eq!(params.contract.width, 1280);
    assert!(params.backends.effect.is_none() && params.backends.chains.is_empty());
    assert_eq!(params.effect.mode, EffectMode::None);
    assert_eq!(
        h.snap().warning,
        "Output 1280x720@30 could not start with its effects (obc-module: no CUDA memory): \
         streaming without them"
    );
    assert!(h.ctrl.failed_contract.is_none(), "the contract works");
    // The next session tries with them again.
    h.world().fail_start_effects = None;
    h.world().fail_start_contract = false;
    h.consumers(0, 12_000);
    h.tick(20_000);
    assert_eq!(h.snap().warning, "");
    h.activate(21_000);
    assert!(h.session_params().backends.effect.is_some());
    // Without effects to drop, or failing without them too: the error, as
    // before.
    h.consumers(0, 22_000);
    h.tick(30_000);
    h.world().fail_start = Some("CUDA initialisation failed".into());
    h.consumers(1, 31_000);
    h.tick(31_300);
    assert_eq!(h.state(), State::Error);
}

/// A session streaming without its effects ([`super::Controller::passthrough`])
/// after its output could not start with them: `config`, the effect
/// element's start failing.
fn streaming_without_effects(config: &str) -> H {
    let mut h = H::new(config, |w| {
        w.fail_start_effects = Some("obc-module: no CUDA memory".into());
    });
    h.activate(0);
    let params = h.session_params();
    assert!(params.backends.effect.is_none() && params.backends.chains.is_empty());
    assert_eq!(params.effect.mode, EffectMode::None);
    assert!(h.snap().warning.contains("streaming without them"));
    h
}

#[test]
fn a_blur_switched_on_in_a_session_without_its_effects_is_never_shown_raw() {
    // No effect selected (`black`, the default): the session streams
    // without the effect element.
    let mut h = streaming_without_effects("");
    let starts = h.world().session_starts;
    // Blur switched on: never the raw picture. The session is started
    // again with the effect; it still cannot start, and under `black` it
    // is not started without it.
    let _ = h.set_effect("blur", 1000);
    assert!(h.world().session.is_none(), "{:?}", h.snap());
    assert_eq!(h.world().session_starts, starts);
    assert_eq!(h.state(), State::Error);
    // The cause gone: the effect runs.
    h.world().fail_start_effects = None;
    h.consumers(0, 2000);
    h.tick(10_000);
    h.activate(11_000);
    let params = h.session_params();
    assert!(params.backends.effect.is_some());
    assert_eq!(params.effect.mode, EffectMode::Blur);
    assert_eq!(h.snap().warning, "");
}

#[test]
fn a_reload_to_black_stops_a_session_that_streams_without_its_effect() {
    let none = "[effect]\nmode = \"blur\"\non_failure = \"none\"\n";
    let black = "[effect]\nmode = \"blur\"\non_failure = \"black\"\n";
    // Under `none` the raw picture is the user's choice.
    let mut h = streaming_without_effects(none);
    h.reload(black, 1000).unwrap();
    assert!(h.world().session.is_none(), "{:?}", h.snap());
    assert_eq!(h.state(), State::Error);
    // However `black` comes to be in force, the next step stops such a
    // session.
    let mut h = streaming_without_effects(none);
    h.ctrl.config.effect.on_failure = EffectOnFailure::Black;
    h.tick(1000);
    assert!(h.world().session.is_none(), "{:?}", h.snap());
    // No effect selected: nothing is withheld, the session stands.
    let mut h = streaming_without_effects(none);
    h.ctrl.config.effect.on_failure = EffectOnFailure::Black;
    h.ctrl.config.effect.mode = EffectMode::None;
    h.tick(1000);
    assert!(h.world().session.is_some());
}

#[test]
fn a_control_call_retries_the_effects_of_a_session_without_them_under_none() {
    let mut h = streaming_without_effects("[effect]\nmode = \"blur\"\non_failure = \"none\"\n");
    // A control call tries them again; still failing, the session streams
    // without them once more (`none`): the feed is kept.
    let ingests = h.world().ingests.len();
    h.set_effect("blur", 500).unwrap();
    assert_eq!(h.state(), State::Active, "{:?}", h.snap());
    assert_eq!(h.world().ingests.len(), ingests + 1, "the camera reopened");
    assert!(h.session_params().backends.effect.is_none());
    assert!(h.snap().warning.contains("streaming without them"));
    // The cause gone, a control call starts the session again with them.
    h.world().fail_start_effects = None;
    let starts = h.world().session_starts;
    h.set_effect("blur", 1000).unwrap();
    h.tick(1300);
    assert_eq!(h.world().session_starts, starts + 1);
    assert_eq!(h.state(), State::Active, "{:?}", h.snap());
    let params = h.session_params();
    assert!(params.backends.effect.is_some());
    assert_eq!(params.effect.mode, EffectMode::Blur);
    assert_eq!(h.snap().warning, "");
}

#[test]
fn a_blur_with_black_on_failure_is_never_dropped_to_start_the_output() {
    let mut h = H::new(
        "[effect]\nmode = \"blur\"\non_failure = \"black\"\n",
        |_| {},
    );
    h.world().fail_start_effects = Some("obc-module: no CUDA memory".into());
    h.consumers(1, 0);
    h.tick(300);
    assert_eq!(h.state(), State::Error);
    assert_eq!(h.world().session_starts, 0);
}

// ---- the fallback image (`on_failure = "image"`) ----

const AWAY: &str = "/srv/away.png";

fn image_config(extra: &str) -> String {
    format!(
        "[effect]\nmode = \"blur\"\non_failure = \"image\"\nfallback_image = \"{AWAY}\"\n{extra}"
    )
}

fn placeholder_sets(h: &H) -> Vec<Option<PathBuf>> {
    h.world().placeholder_sets.clone()
}

#[test]
fn a_start_refused_under_image_shows_the_fallback_image_until_the_effect_is_off() {
    let mut h = H::new(&image_config(""), |w| {
        w.fail_start_effects = Some("obc-module: no CUDA memory".into())
    });
    h.consumers(1, 0);
    h.tick(300);
    // As under `black`: never streamed without the effect.
    assert_eq!(h.state(), State::Error);
    assert_eq!(h.world().session_starts, 0);
    assert!(h.world().session.is_none());
    // The writer's placeholder shows the image meanwhile.
    assert_eq!(h.world().placeholder, Some(PathBuf::from(AWAY)));
    assert_eq!(placeholder_sets(&h), [Some(PathBuf::from(AWAY))]);
    assert_eq!(h.snap().warning, "", "the image can be read");
    // Further steps change nothing.
    h.tick(1000);
    assert_eq!(placeholder_sets(&h).len(), 1);
    // The effect turned off: the session starts without it, and the
    // placeholder is black again.
    h.set_effect("none", 1500).unwrap();
    h.tick(1800);
    assert!(h.world().session.is_some(), "{:?}", h.snap());
    assert_eq!(h.world().placeholder, None);
    assert_eq!(
        placeholder_sets(&h),
        [Some(PathBuf::from(AWAY)), None],
        "switched once each way"
    );
}

#[test]
fn a_start_refused_under_image_goes_black_again_when_the_camera_stops() {
    let mut h = H::new(&image_config(""), |w| {
        w.fail_start_effects = Some("obc-module: no CUDA memory".into())
    });
    h.consumers(1, 0);
    h.tick(300);
    assert_eq!(h.world().placeholder, Some(PathBuf::from(AWAY)));
    // The application closes the virtual camera: idle, black.
    h.consumers(0, 1000);
    h.tick(1200);
    assert_eq!(h.state(), State::Idle, "{:?}", h.snap());
    assert_eq!(h.world().placeholder, None);
    // Opened again: refused again, the image again.
    h.consumers(1, 2000);
    h.tick(2300);
    assert_eq!(h.state(), State::Error);
    assert_eq!(h.world().placeholder, Some(PathBuf::from(AWAY)));
    // Turned off: black.
    h.call(|r| Command::SetEnabled(false, r), 3000).unwrap();
    assert_eq!(h.state(), State::Off);
    assert_eq!(h.world().placeholder, None);
    assert_eq!(
        placeholder_sets(&h),
        [
            Some(PathBuf::from(AWAY)),
            None,
            Some(PathBuf::from(AWAY)),
            None
        ]
    );
}

#[test]
fn black_keeps_a_refused_start_black_and_other_faults_stay_black() {
    // `black`: the placeholder is never switched.
    let mut h = H::new(
        &format!(
            "[effect]\nmode = \"blur\"\non_failure = \"black\"\nfallback_image = \"{AWAY}\"\n"
        ),
        |w| w.fail_start_effects = Some("obc-module: no CUDA memory".into()),
    );
    h.consumers(1, 0);
    h.tick(300);
    assert_eq!(h.state(), State::Error);
    assert!(placeholder_sets(&h).is_empty());
    // `image`, but a fault that is not the effect's (no camera): black.
    let mut h = H::new(&image_config(""), |w| w.cameras.clear());
    h.consumers(1, 0);
    h.tick(300);
    assert_eq!(h.state(), State::Error, "{:?}", h.snap());
    assert!(placeholder_sets(&h).is_empty());
}

#[test]
fn a_replace_that_cannot_read_its_background_under_image_shows_the_fallback_image() {
    // The settings fault found before the camera opens.
    let mut h = H::new(
        &format!(
            "[effect]\nmode = \"replace\"\nbackground_image = \"/nowhere/bg.png\"\n\
             on_failure = \"image\"\nfallback_image = \"{AWAY}\"\n"
        ),
        |_| {},
    );
    h.consumers(1, 0);
    h.tick(300);
    assert_eq!(h.state(), State::Error);
    assert!(
        h.snap().error.contains("effect replace unavailable"),
        "{:?}",
        h.snap()
    );
    assert_eq!(h.world().placeholder, Some(PathBuf::from(AWAY)));
}

#[test]
fn an_unreadable_fallback_image_is_black_with_a_warning() {
    let mut h = H::new(&image_config(""), |w| {
        w.fail_start_effects = Some("obc-module: no CUDA memory".into());
        w.placeholder_unreadable = Some("the image file is unreadable: gone".into());
    });
    h.consumers(1, 0);
    h.tick(300);
    assert_eq!(h.state(), State::Error);
    // Asked for: the writer shows black in its place, and says why.
    assert_eq!(h.world().placeholder, Some(PathBuf::from(AWAY)));
    let warning = h.snap().warning;
    assert_eq!(
        warning,
        format!(
            "Fallback image {AWAY} could not be read (the image file is unreadable: gone): \
             showing black"
        )
    );
    // Once the image is no longer asked for, nothing to say.
    h.world().fail_start_effects = None;
    h.set_effect("none", 1000).unwrap();
    h.tick(1300);
    assert_eq!(h.world().placeholder, None);
    assert_eq!(h.snap().warning, "");
}

#[test]
fn image_without_a_fallback_image_is_black_with_a_warning() {
    const NO_IMAGE: &str =
        "effect.on_failure is \"image\" but effect.fallback_image is not set: showing black";
    let mut h = H::new("[effect]\nmode = \"blur\"\non_failure = \"image\"\n", |w| {
        w.fail_start_effects = Some("obc-module: no CUDA memory".into())
    });
    // Said as soon as it is configured.
    assert_eq!(h.snap().warning, NO_IMAGE);
    h.consumers(1, 0);
    h.tick(300);
    // As `black`: no session, the placeholder black.
    assert_eq!(h.state(), State::Error);
    assert_eq!(h.world().session_starts, 0);
    assert!(placeholder_sets(&h).is_empty());
    assert_eq!(h.snap().warning, NO_IMAGE);
    // The session is given `image` with no image: the element shows black.
    let mut h = H::new(
        "[effect]\nmode = \"blur\"\non_failure = \"image\"\n",
        |_| {},
    );
    h.activate(0);
    let e = h.world().effect.clone().unwrap();
    assert_eq!(
        (e.on_failure, e.fallback_image),
        (EffectOnFailure::Image, None)
    );
}

#[test]
fn a_session_under_image_gives_the_element_the_image_and_reports_what_it_cannot_read() {
    let mut h = H::new(&image_config(""), |w| w.effect_slow = true);
    h.activate(0);
    let e = h.world().effect.clone().unwrap();
    assert_eq!(
        (e.on_failure, e.fallback_image),
        (EffectOnFailure::Image, Some(PathBuf::from(AWAY)))
    );
    // The session runs: the placeholder is not involved.
    assert!(placeholder_sets(&h).is_empty());
    // The element cannot read it: said only while the effect has failed
    // (until then the image is not what shows).
    let why = format!("Fallback image {AWAY} could not be read (no such file): showing black");
    h.world().fallback_error = Some(why.clone());
    h.world().effect_status = EffectStatus::Ready;
    h.tick(500);
    assert_eq!(h.snap().warning, "");
    h.world().effect_status = EffectStatus::Failed("processing a frame failed".into());
    h.tick(1000);
    assert_eq!(h.state(), State::Active);
    let warning = h.snap().warning;
    assert_eq!(
        warning,
        format!(
            "effect blur unavailable: processing a frame failed; the fallback image is shown \
             until it can run; {why}"
        )
    );
}

#[test]
fn a_reload_to_image_stops_a_session_without_its_effect_and_shows_the_image() {
    let none = "[effect]\nmode = \"blur\"\non_failure = \"none\"\n";
    let mut h = streaming_without_effects(none);
    assert!(placeholder_sets(&h).is_empty());
    h.reload(&image_config(""), 1000).unwrap();
    assert!(h.world().session.is_none(), "{:?}", h.snap());
    assert_eq!(h.state(), State::Error);
    assert_eq!(h.world().placeholder, Some(PathBuf::from(AWAY)));
}
