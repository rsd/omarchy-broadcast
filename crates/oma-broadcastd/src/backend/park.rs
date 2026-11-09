//! Parking the GPU while the daemon idles.
//!
//! The first session initialises CUDA in the daemon (GStreamer's CUDA
//! library calls `cuInit`), and from then on `nvidia-uvm` keeps the GPU
//! registered, so it cannot runtime-suspend, even with every context gone.
//! [`Parker::park`], called once a session has stopped and nothing in the
//! process uses CUDA any more, checkpoints the process's CUDA state
//! ([`obc_gpu::park`]): the daemon then holds no GPU device. The next
//! session's start calls [`Parker::unpark`] first ([`obc_gpu::unpark`]),
//! before anything touches CUDA: while parked, a CUDA call would block.
//!
//! The policy, whatever the driver says:
//!
//! * a park that fails leaves the GPU awake, as it was before parking
//!   existed, with a warning; the daemon works on, and the next idle tries
//!   again (an error of the call, `CUDA_ERROR_NOT_SUPPORTED` included, is
//!   that park's only);
//! * a park that fails and leaves the process locked (the checkpoint failed,
//!   and so did unlocking again) is warned as such: the next start unlocks
//!   it first, like an unpark;
//! * a driver that cannot park at all (no driver library, or one without
//!   the checkpoint API) is said once, at info, and never asked again;
//! * an unpark that fails is an error for the session start: nothing may
//!   touch CUDA, so no session can run (the caller fails closed);
//! * a process that never initialised CUDA has nothing to park;
//! * a CUDA call `obc_gpu` refused while parked (it would have blocked) is
//!   a caller that ran when it should not have: warned once per park.
//!
//! [`Parker::stop_session`] and [`Parker::start_session`] are the session
//! lifecycle around it, over any [`StopSession`]: the GStreamer backend's
//! order, unit-tested here with a recording session.

use std::time::Duration;

use obc_gpu::{GpuError, NothingToPark, ParkError, ParkOutcome, UnparkOutcome};

use crate::log::Level;

/// The driver calls a [`Parker`] makes: [`GpuOps`] in the daemon, a script
/// in the tests.
pub trait ParkOps: Send {
    fn park(&mut self) -> Result<ParkOutcome, ParkError>;
    fn unpark(&mut self) -> Result<UnparkOutcome, GpuError>;
    /// How many CUDA calls were refused while parked, ever
    /// ([`obc_gpu::parked_refusals`]).
    fn refusals(&self) -> u64;
}

/// The daemon's own process, through [`obc_gpu::park`] and
/// [`obc_gpu::unpark`].
#[derive(Debug, Default, Clone, Copy)]
pub struct GpuOps;

impl ParkOps for GpuOps {
    fn park(&mut self) -> Result<ParkOutcome, ParkError> {
        obc_gpu::park()
    }

    fn unpark(&mut self) -> Result<UnparkOutcome, GpuError> {
        obc_gpu::unpark()
    }

    fn refusals(&self) -> u64 {
        obc_gpu::parked_refusals()
    }
}

/// A running session, as the [`Parker`] lifecycle sees it.
pub trait StopSession {
    /// Stops all of the session and drops it. `Ok` means it stopped
    /// cleanly: once this returns, nothing of it uses CUDA. `Err` (a
    /// pipeline that did not reach NULL, say) means something of it may
    /// still be alive and call CUDA.
    fn stop(self) -> Result<(), String>;
}

/// Where a [`Parker`] logs: [`crate::log::line`] in the daemon.
pub type ParkLog = Box<dyn Fn(Level, &str) + Send>;

/// The park state machine (see the module docs).
pub struct Parker<O: ParkOps = GpuOps> {
    ops: O,
    enabled: bool,
    /// The driver said it cannot park: said once, never asked again.
    unsupported: bool,
    /// The last park failed and left the process locked: the next unpark
    /// unlocks it.
    blocked: bool,
    /// [`ParkOps::refusals`] when last looked at.
    refusals_seen: u64,
    log: ParkLog,
    last_park: Option<Duration>,
    last_unpark: Option<Duration>,
}

impl<O: ParkOps + std::fmt::Debug> std::fmt::Debug for Parker<O> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Parker")
            .field("ops", &self.ops)
            .field("enabled", &self.enabled)
            .field("unsupported", &self.unsupported)
            .field("blocked", &self.blocked)
            .field("refusals_seen", &self.refusals_seen)
            .field("last_park", &self.last_park)
            .field("last_unpark", &self.last_unpark)
            .finish_non_exhaustive()
    }
}

impl Parker<GpuOps> {
    /// The daemon's parker: its own process, logging to the journal.
    pub fn new() -> Self {
        Parker::with(GpuOps, Box::new(crate::log::line))
    }
}

impl Default for Parker<GpuOps> {
    fn default() -> Self {
        Parker::new()
    }
}

impl<O: ParkOps> Parker<O> {
    /// A parker over `ops`, logging to `log`. Parking is on.
    pub fn with(ops: O, log: ParkLog) -> Self {
        let refusals_seen = ops.refusals();
        Parker {
            ops,
            enabled: true,
            unsupported: false,
            blocked: false,
            refusals_seen,
            log,
            last_park: None,
            last_unpark: None,
        }
    }

    /// Turns parking on or off (on by default). Off, [`Self::park`] does
    /// nothing, as before parking existed; [`Self::unpark`] still restores
    /// a process parked earlier. A measurement hook (the hardware tests
    /// compare restarts with and without parking).
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
    }

    /// How long the last park that parked took (lock and checkpoint).
    pub fn last_park(&self) -> Option<Duration> {
        self.last_park
    }

    /// How long the last unpark that unparked took (restore and unlock).
    pub fn last_unpark(&self) -> Option<Duration> {
        self.last_unpark
    }

    /// Parks the GPU, if it can be: call it once nothing in the process
    /// uses CUDA. Never fails: a failure leaves the GPU awake, or, said as
    /// such, locked until the next [`Self::unpark`] (warned either way).
    pub fn park(&mut self) {
        self.note_refusals();
        if !self.enabled || self.unsupported {
            return;
        }
        match self.ops.park() {
            Ok(ParkOutcome::Parked { lock, checkpoint }) => {
                let total = lock + checkpoint;
                self.last_park = Some(total);
                (self.log)(
                    Level::Info,
                    &format!(
                        "GPU parked in {} ms (lock {} ms, checkpoint {} ms): it can power down \
                         while idle",
                        total.as_millis(),
                        lock.as_millis(),
                        checkpoint.as_millis()
                    ),
                );
            }
            Ok(ParkOutcome::NothingToPark(why)) => (self.log)(
                Level::Debug,
                match why {
                    NothingToPark::NotInitialised => {
                        "GPU not parked: CUDA was never initialised in the daemon"
                    }
                    NothingToPark::AlreadyParked => "GPU already parked",
                },
            ),
            Ok(ParkOutcome::Unsupported(reason)) => self.unsupported(&reason),
            Err(ParkError::Awake(e)) => (self.log)(
                Level::Warning,
                &format!("cannot park the GPU, it stays awake while idle: {e}"),
            ),
            Err(ParkError::Blocked(e)) => {
                self.blocked = true;
                (self.log)(
                    Level::Warning,
                    &format!(
                        "cannot park the GPU, and the daemon's CUDA state is left locked: no \
                         CUDA call can run until the next session start unlocks it: {e}"
                    ),
                );
            }
        }
    }

    /// Unparks the GPU if it is parked (or locked, by a failed park or
    /// another tool): call it before anything touches CUDA. An error means
    /// CUDA cannot be used (it would block): the session must not start.
    pub fn unpark(&mut self) -> Result<(), String> {
        // Asked even when parking is off or unsupported: a process left
        // parked (or locked) by an earlier attempt, or by another tool, is
        // restored all the same, and one that is running costs a state
        // query.
        let res = match self.ops.unpark() {
            Ok(UnparkOutcome::Unparked { restore, unlock }) => {
                let total = restore + unlock;
                self.last_unpark = Some(total);
                self.blocked = false;
                (self.log)(
                    Level::Info,
                    &format!(
                        "GPU unparked in {} ms (restore {} ms, unlock {} ms)",
                        total.as_millis(),
                        restore.as_millis(),
                        unlock.as_millis()
                    ),
                );
                Ok(())
            }
            Ok(UnparkOutcome::NotParked) => {
                self.blocked = false;
                Ok(())
            }
            Ok(UnparkOutcome::Unsupported(reason)) => {
                self.unsupported(&reason);
                Ok(())
            }
            Err(e) if self.blocked => Err(format!(
                "the GPU could not be unlocked after a failed park: {e}; restart the daemon"
            )),
            Err(e) => Err(format!(
                "the GPU could not be restored from its parked state: {e}; restart the daemon"
            )),
        };
        self.note_refusals();
        res
    }

    /// Runs `start` (a session start, which touches CUDA) with the GPU
    /// unparked first. If unparking fails, `start` does not run and that is
    /// the error; if `start` fails, the GPU is parked again, so a failed
    /// start does not keep it awake.
    pub fn starting<T>(&mut self, start: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        self.unpark()?;
        let res = start();
        if res.is_err() {
            self.park();
        }
        res
    }

    /// Stops the session in `running`, if any, all of it, and parks the
    /// GPU only then, and only if it stopped cleanly: a session that did
    /// not may still use CUDA, which would block once parked, so the GPU
    /// stays awake (warned).
    pub fn stop_session<S: StopSession>(&mut self, running: &mut Option<S>) {
        let Some(session) = running.take() else {
            return;
        };
        match session.stop() {
            Ok(()) => self.park(),
            Err(e) => (self.log)(
                Level::Warning,
                &format!(
                    "stopping the session: {e}; not parking the GPU (something of the session \
                     may still use it): it stays awake while idle"
                ),
            ),
        }
    }

    /// Starts a session in `running` with `build`, which touches CUDA: a
    /// session running is stopped first, without parking in between (the
    /// GPU is in use again at once), then [`Self::starting`] unparks the
    /// GPU, builds, and parks it again if the build fails. On error
    /// `running` is empty.
    pub fn start_session<S: StopSession>(
        &mut self,
        running: &mut Option<S>,
        build: impl FnOnce() -> Result<S, String>,
    ) -> Result<(), String> {
        if let Some(session) = running.take() {
            (self.log)(Level::Debug, "session restarted: not parking in between");
            if let Err(e) = session.stop() {
                (self.log)(Level::Warning, &format!("stopping the session: {e}"));
            }
        }
        *running = Some(self.starting(build)?);
        Ok(())
    }

    /// Warns about CUDA calls refused since last looked at.
    fn note_refusals(&mut self) {
        let n = self.ops.refusals();
        if n > self.refusals_seen {
            (self.log)(
                Level::Warning,
                &format!(
                    "{} CUDA call(s) were refused while the GPU was parked (they failed instead \
                     of blocking): something used the GPU while the daemon was idle",
                    n - self.refusals_seen
                ),
            );
        }
        self.refusals_seen = n;
    }

    fn unsupported(&mut self, reason: &str) {
        if !self.unsupported {
            self.unsupported = true;
            (self.log)(
                Level::Info,
                &format!(
                    "the GPU cannot be parked while idle ({reason}): after a session it stays \
                     awake until the daemon exits"
                ),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use super::*;

    /// Scripted driver calls: each queue gives the next result (a call
    /// with an empty queue panics), and every call is counted. `events`
    /// records the calls, and what a [`Rec`] session and a build did, in
    /// order.
    #[derive(Default)]
    struct Script {
        park: VecDeque<Result<ParkOutcome, ParkError>>,
        unpark: VecDeque<Result<UnparkOutcome, GpuError>>,
        parks: u32,
        unparks: u32,
        refusals: u64,
        events: Vec<&'static str>,
    }

    #[derive(Clone, Default)]
    struct ScriptOps(Arc<Mutex<Script>>);

    impl ParkOps for ScriptOps {
        fn park(&mut self) -> Result<ParkOutcome, ParkError> {
            let mut s = self.0.lock().unwrap();
            s.parks += 1;
            s.events.push("park");
            s.park.pop_front().expect("an unscripted park")
        }

        fn unpark(&mut self) -> Result<UnparkOutcome, GpuError> {
            let mut s = self.0.lock().unwrap();
            s.unparks += 1;
            s.events.push("unpark");
            s.unpark.pop_front().expect("an unscripted unpark")
        }

        fn refusals(&self) -> u64 {
            self.0.lock().unwrap().refusals
        }
    }

    impl ScriptOps {
        fn events(&self) -> Vec<&'static str> {
            std::mem::take(&mut self.0.lock().unwrap().events)
        }

        fn event(&self, e: &'static str) {
            self.0.lock().unwrap().events.push(e);
        }
    }

    /// A recording session: its stop is an event, and fails with `fail`.
    struct Rec {
        ops: ScriptOps,
        fail: Option<String>,
    }

    impl StopSession for Rec {
        fn stop(self) -> Result<(), String> {
            self.ops.event("stop");
            self.fail.map_or(Ok(()), Err)
        }
    }

    fn rec(ops: &ScriptOps) -> Rec {
        Rec {
            ops: ops.clone(),
            fail: None,
        }
    }

    type Logged = Arc<Mutex<Vec<(Level, String)>>>;

    fn parker() -> (Parker<ScriptOps>, ScriptOps, Logged) {
        let ops = ScriptOps::default();
        let logged: Logged = Arc::default();
        let l = Arc::clone(&logged);
        let p = Parker::with(
            ops.clone(),
            Box::new(move |level, msg| l.lock().unwrap().push((level, msg.to_string()))),
        );
        (p, ops, logged)
    }

    fn take(logged: &Logged) -> Vec<(Level, String)> {
        std::mem::take(&mut logged.lock().unwrap())
    }

    const PARKED: ParkOutcome = ParkOutcome::Parked {
        lock: Duration::from_millis(1),
        checkpoint: Duration::from_millis(80),
    };
    const UNPARKED: UnparkOutcome = UnparkOutcome::Unparked {
        restore: Duration::from_millis(60),
        unlock: Duration::from_millis(2),
    };

    #[test]
    fn a_park_and_an_unpark_are_logged_with_their_durations() {
        let (mut p, ops, logged) = parker();
        {
            let mut s = ops.0.lock().unwrap();
            s.park.push_back(Ok(PARKED));
            s.unpark.push_back(Ok(UNPARKED));
        }
        p.park();
        assert_eq!(p.last_park(), Some(Duration::from_millis(81)));
        let l = take(&logged);
        assert_eq!(l.len(), 1, "{l:?}");
        assert_eq!(l[0].0, Level::Info);
        assert!(l[0].1.starts_with("GPU parked in 81 ms"), "{}", l[0].1);
        assert_eq!(p.unpark(), Ok(()));
        assert_eq!(p.last_unpark(), Some(Duration::from_millis(62)));
        let l = take(&logged);
        assert_eq!(l[0].0, Level::Info);
        assert!(l[0].1.starts_with("GPU unparked in 62 ms"), "{}", l[0].1);
    }

    #[test]
    fn a_process_that_never_initialised_cuda_parks_nothing_and_says_so_only_in_debug() {
        let (mut p, ops, logged) = parker();
        {
            let mut s = ops.0.lock().unwrap();
            s.park.push_back(Ok(ParkOutcome::NothingToPark(
                NothingToPark::NotInitialised,
            )));
            s.unpark.push_back(Ok(UnparkOutcome::NotParked));
        }
        p.park();
        assert_eq!(p.unpark(), Ok(()));
        assert_eq!(p.last_park(), None);
        let l = take(&logged);
        assert!(l.iter().all(|(lv, _)| *lv == Level::Debug), "{l:?}");
    }

    #[test]
    fn a_failed_park_leaves_the_gpu_awake_with_a_warning_and_parks_again_next_time() {
        let (mut p, ops, logged) = parker();
        {
            let mut s = ops.0.lock().unwrap();
            s.park.push_back(Err(ParkError::Awake(GpuError::Unavailable(
                "lock timed out".into(),
            ))));
            s.park.push_back(Ok(PARKED));
            s.unpark.push_back(Ok(UnparkOutcome::NotParked));
        }
        p.park();
        let l = take(&logged);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].0, Level::Warning);
        assert!(l[0].1.contains("stays awake"), "{}", l[0].1);
        assert!(l[0].1.contains("lock timed out"), "{}", l[0].1);
        // Not parked: the next start has nothing to restore.
        assert_eq!(p.unpark(), Ok(()));
        // A later idle tries again.
        p.park();
        assert_eq!(ops.0.lock().unwrap().parks, 2);
        assert!(take(&logged)[0].1.starts_with("GPU parked"));
    }

    #[test]
    fn an_unsupported_driver_is_said_once_and_never_asked_again() {
        let (mut p, ops, logged) = parker();
        {
            let mut s = ops.0.lock().unwrap();
            s.park.push_back(Ok(ParkOutcome::Unsupported(
                "libcuda.so.1 has no cuCheckpointProcessLock".into(),
            )));
            for _ in 0..3 {
                s.unpark.push_back(Ok(UnparkOutcome::NotParked));
            }
        }
        for _ in 0..3 {
            p.park();
            assert_eq!(p.unpark(), Ok(()));
        }
        assert_eq!(ops.0.lock().unwrap().parks, 1, "asked once");
        let l = take(&logged);
        assert_eq!(l.len(), 1, "{l:?}");
        assert_eq!(l[0].0, Level::Info);
        assert!(
            l[0].1.contains("cannot be parked") && l[0].1.contains("cuCheckpointProcessLock"),
            "{}",
            l[0].1
        );
    }

    #[test]
    fn an_unpark_that_fails_is_an_error_naming_the_cause() {
        let (mut p, ops, logged) = parker();
        {
            let mut s = ops.0.lock().unwrap();
            s.park.push_back(Ok(PARKED));
            s.unpark.push_back(Err(GpuError::Call {
                call: "cuCheckpointProcessRestore",
                code: 2,
                name: "CUDA_ERROR_OUT_OF_MEMORY".into(),
                description: "out of memory".into(),
            }));
            s.unpark.push_back(Ok(UNPARKED));
        }
        p.park();
        take(&logged);
        let e = p.unpark().unwrap_err();
        assert!(
            e.starts_with("the GPU could not be restored from its parked state"),
            "{e}"
        );
        assert!(e.contains("CUDA_ERROR_OUT_OF_MEMORY"), "{e}");
        assert!(e.ends_with("restart the daemon"), "{e}");
        assert!(take(&logged).is_empty(), "the caller reports it");
        // Still parked: the next start tries again.
        assert_eq!(p.unpark(), Ok(()));
        assert_eq!(ops.0.lock().unwrap().unparks, 2);
    }

    #[test]
    fn a_start_runs_unparked_and_a_failed_start_parks_again() {
        let (mut p, ops, _logged) = parker();
        {
            let mut s = ops.0.lock().unwrap();
            s.park.push_back(Ok(PARKED));
            s.unpark.push_back(Ok(UNPARKED));
            s.park.push_back(Ok(PARKED));
            s.unpark.push_back(Ok(UNPARKED));
        }
        p.park();
        // The start sees the GPU unparked.
        let seen = ops.clone();
        let e = p
            .starting(|| -> Result<(), String> {
                assert_eq!(
                    seen.0.lock().unwrap().unparks,
                    1,
                    "unparked before the start"
                );
                Err("output pipeline: no cudaupload".into())
            })
            .unwrap_err();
        assert_eq!(e, "output pipeline: no cudaupload");
        assert_eq!(ops.0.lock().unwrap().parks, 2, "parked again");
        // A start that works stays unparked.
        assert_eq!(p.starting(|| Ok(7)), Ok(7));
        let s = ops.0.lock().unwrap();
        assert_eq!((s.parks, s.unparks), (2, 2));
    }

    #[test]
    fn a_start_whose_unpark_fails_does_not_run() {
        let (mut p, ops, _logged) = parker();
        ops.0
            .lock()
            .unwrap()
            .unpark
            .push_back(Err(GpuError::Unavailable("state Failed".into())));
        let e = p
            .starting(|| -> Result<(), String> { panic!("started while parked") })
            .unwrap_err();
        assert!(e.contains("state Failed"), "{e}");
        assert_eq!(ops.0.lock().unwrap().parks, 0, "nothing to park again");
    }

    #[test]
    fn parking_off_parks_nothing_but_still_unparks() {
        let (mut p, ops, _logged) = parker();
        ops.0.lock().unwrap().unpark.push_back(Ok(UNPARKED));
        p.set_enabled(false);
        p.park();
        assert_eq!(ops.0.lock().unwrap().parks, 0);
        assert_eq!(p.unpark(), Ok(()));
        assert_eq!(ops.0.lock().unwrap().unparks, 1);
    }

    #[test]
    fn a_park_that_leaves_the_process_locked_says_so_and_the_next_start_unlocks_it() {
        let (mut p, ops, logged) = parker();
        {
            let mut s = ops.0.lock().unwrap();
            s.park
                .push_back(Err(ParkError::Blocked(GpuError::Unavailable(
                    "checkpoint failed; unlocking again failed too: the process stays locked"
                        .into(),
                ))));
            s.unpark.push_back(Ok(UnparkOutcome::Unparked {
                restore: Duration::ZERO,
                unlock: Duration::from_millis(1),
            }));
        }
        p.park();
        let l = take(&logged);
        assert_eq!(l.len(), 1, "{l:?}");
        assert_eq!(l[0].0, Level::Warning);
        assert!(l[0].1.starts_with("cannot park the GPU"), "{}", l[0].1);
        assert!(l[0].1.contains("left locked"), "{}", l[0].1);
        assert!(!l[0].1.contains("stays awake"), "it is locked: {}", l[0].1);
        // The next start unlocks before building.
        assert_eq!(
            p.starting(|| {
                ops.event("build");
                Ok(())
            }),
            Ok(())
        );
        assert_eq!(ops.events(), ["park", "unpark", "build"]);
        assert_eq!(p.last_unpark(), Some(Duration::from_millis(1)));
    }

    #[test]
    fn an_unlock_that_fails_after_a_locked_park_is_an_error_naming_it() {
        let (mut p, ops, _logged) = parker();
        {
            let mut s = ops.0.lock().unwrap();
            s.park
                .push_back(Err(ParkError::Blocked(GpuError::Unavailable(
                    "stays locked".into(),
                ))));
            s.unpark
                .push_back(Err(GpuError::Unavailable("unlock failed".into())));
        }
        p.park();
        let e = p.unpark().unwrap_err();
        assert!(
            e.starts_with("the GPU could not be unlocked after a failed park")
                && e.contains("unlock failed")
                && e.ends_with("restart the daemon"),
            "{e}"
        );
    }

    #[test]
    fn a_call_the_driver_does_not_support_fails_that_park_only() {
        let (mut p, ops, logged) = parker();
        {
            let mut s = ops.0.lock().unwrap();
            s.park.push_back(Err(ParkError::Awake(GpuError::Call {
                call: "cuCheckpointProcessCheckpoint",
                code: 801,
                name: "CUDA_ERROR_NOT_SUPPORTED".into(),
                description: "operation not supported".into(),
            })));
            s.park.push_back(Ok(PARKED));
        }
        p.park();
        let l = take(&logged);
        assert_eq!(l[0].0, Level::Warning, "{l:?}");
        assert!(
            l[0].1.contains("stays awake") && l[0].1.contains("CUDA_ERROR_NOT_SUPPORTED"),
            "{}",
            l[0].1
        );
        // Not "unsupported for ever": the next idle asks again.
        p.park();
        assert_eq!(ops.0.lock().unwrap().parks, 2);
        assert!(take(&logged)[0].1.starts_with("GPU parked"));
    }

    #[test]
    fn calls_refused_while_parked_are_warned_once_per_park() {
        let (mut p, ops, logged) = parker();
        {
            let mut s = ops.0.lock().unwrap();
            s.park.push_back(Ok(PARKED));
            s.unpark.push_back(Ok(UNPARKED));
            s.unpark.push_back(Ok(UnparkOutcome::NotParked));
        }
        p.park();
        take(&logged);
        ops.0.lock().unwrap().refusals = 2;
        assert_eq!(p.unpark(), Ok(()));
        let l = take(&logged);
        let warned: Vec<_> = l.iter().filter(|(lv, _)| *lv == Level::Warning).collect();
        assert_eq!(warned.len(), 1, "{l:?}");
        assert!(
            warned[0]
                .1
                .starts_with("2 CUDA call(s) were refused while the GPU was parked"),
            "{}",
            warned[0].1
        );
        // Said once: nothing new, nothing said.
        assert_eq!(p.unpark(), Ok(()));
        assert!(take(&logged).is_empty());
    }

    #[test]
    fn a_parker_counts_only_the_refusals_after_it_was_made() {
        let ops = ScriptOps::default();
        ops.0.lock().unwrap().refusals = 5;
        ops.0
            .lock()
            .unwrap()
            .unpark
            .push_back(Ok(UnparkOutcome::NotParked));
        let logged: Logged = Arc::default();
        let l = Arc::clone(&logged);
        let mut p = Parker::with(
            ops.clone(),
            Box::new(move |level, msg| l.lock().unwrap().push((level, msg.to_string()))),
        );
        assert_eq!(p.unpark(), Ok(()));
        assert!(take(&logged).is_empty());
    }

    // ---- the session lifecycle (GstBackend's order) ----

    #[test]
    fn a_stop_stops_all_of_the_session_before_parking() {
        let (mut p, ops, logged) = parker();
        ops.0.lock().unwrap().park.push_back(Ok(PARKED));
        let mut running = Some(rec(&ops));
        p.stop_session(&mut running);
        assert!(running.is_none());
        assert_eq!(ops.events(), ["stop", "park"]);
        assert!(take(&logged).iter().all(|(lv, _)| *lv != Level::Warning));
        // Nothing running: nothing stopped, nothing parked.
        p.stop_session(&mut running);
        assert!(ops.events().is_empty());
    }

    #[test]
    fn a_session_that_did_not_stop_cleanly_leaves_the_gpu_awake() {
        let (mut p, ops, logged) = parker();
        let mut running = Some(Rec {
            ops: ops.clone(),
            fail: Some("output pipeline ended in Paused, not Null".into()),
        });
        p.stop_session(&mut running);
        assert!(running.is_none());
        assert_eq!(ops.events(), ["stop"], "not parked");
        let l = take(&logged);
        assert_eq!(l.len(), 1, "{l:?}");
        assert_eq!(l[0].0, Level::Warning);
        assert!(
            l[0].1.contains("ended in Paused, not Null")
                && l[0].1.contains("not parking the GPU")
                && l[0].1.contains("stays awake"),
            "{}",
            l[0].1
        );
        // Not parked: the next start finds it running and builds.
        ops.0
            .lock()
            .unwrap()
            .unpark
            .push_back(Ok(UnparkOutcome::NotParked));
        let o = ops.clone();
        p.start_session(&mut running, || {
            o.event("build");
            Ok(rec(&o))
        })
        .unwrap();
        assert_eq!(ops.events(), ["unpark", "build"]);
    }

    #[test]
    fn a_start_unparks_before_building_and_parks_nothing_when_it_works() {
        let (mut p, ops, _logged) = parker();
        ops.0.lock().unwrap().unpark.push_back(Ok(UNPARKED));
        let mut running: Option<Rec> = None;
        let o = ops.clone();
        p.start_session(&mut running, || {
            o.event("build");
            Ok(rec(&o))
        })
        .unwrap();
        assert!(running.is_some());
        assert_eq!(ops.events(), ["unpark", "build"]);
    }

    #[test]
    fn a_restart_stops_the_running_session_without_parking_in_between() {
        let (mut p, ops, _logged) = parker();
        ops.0
            .lock()
            .unwrap()
            .unpark
            .push_back(Ok(UnparkOutcome::NotParked));
        let mut running = Some(rec(&ops));
        let o = ops.clone();
        p.start_session(&mut running, || {
            o.event("build");
            Ok(rec(&o))
        })
        .unwrap();
        assert_eq!(ops.events(), ["stop", "unpark", "build"]);
        assert_eq!(ops.0.lock().unwrap().parks, 0);
    }

    #[test]
    fn a_failed_build_parks_again_and_leaves_no_session() {
        let (mut p, ops, _logged) = parker();
        {
            let mut s = ops.0.lock().unwrap();
            s.unpark.push_back(Ok(UNPARKED));
            s.park.push_back(Ok(PARKED));
        }
        let mut running = Some(rec(&ops));
        let o = ops.clone();
        let e = p
            .start_session(&mut running, || -> Result<Rec, String> {
                o.event("build");
                Err("output pipeline: no cudaupload".into())
            })
            .unwrap_err();
        assert_eq!(e, "output pipeline: no cudaupload");
        assert!(running.is_none(), "nothing of the session is left");
        assert_eq!(ops.events(), ["stop", "unpark", "build", "park"]);
    }

    #[test]
    fn a_start_whose_unpark_fails_builds_nothing() {
        let (mut p, ops, _logged) = parker();
        ops.0
            .lock()
            .unwrap()
            .unpark
            .push_back(Err(GpuError::Unavailable("state Failed".into())));
        let mut running: Option<Rec> = None;
        let e = p
            .start_session(&mut running, || -> Result<Rec, String> {
                panic!("built while parked")
            })
            .unwrap_err();
        assert!(e.contains("state Failed"), "{e}");
        assert!(running.is_none());
        assert_eq!(ops.events(), ["unpark"]);
    }
}
