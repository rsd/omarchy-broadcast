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
//! * a session start that failed for want of CUDA ([`StartError::no_cuda`])
//!   is not parked: it left no CUDA context or memory to release, and the
//!   driver's checkpoint lock is not safe there (with `CUDA_VISIBLE_DEVICES`
//!   naming no GPU, it crashed inside `libcuda`), so no driver call is
//!   made;
//! * a CUDA call `obc_gpu` refused while parked (it would have blocked) is
//!   a caller that ran when it should not have: warned once per park;
//! * a session that did not stop cleanly taints the parker for the rest of
//!   the process (see "Tainted" below); a stop that failed otherwise, with
//!   every pipeline in NULL all the same, is warned and parks.
//!
//! **Tainted.** Something of a session that did not stop cleanly (a
//! pipeline left outside NULL) may still be alive and call CUDA, which
//! would block once parked. So any unclean teardown turns parking off
//! until the daemon restarts: a session stop that left a pipeline outside
//! NULL ([`Parker::stop_session`], [`StopFailure::reached_null`]; a camera
//! switch of the session that did counts), the same of the session a
//! restart replaces ([`Parker::start_session`]), and a failed start whose
//! cleanup of what it had built did not stop cleanly ([`StartError`]). The
//! first one is warned, saying so; the GPU then stays awake while idle, as
//! before parking existed. A later clean stop does not clear it: it does
//! not prove the leftovers of the earlier one are gone. Unparking is not
//! affected: a process parked or locked when it got tainted is restored by
//! the next start as usual.
//!
//! Only a pipeline outside NULL taints. A stop that failed with every
//! pipeline in NULL (a camera control that could not be restored, after a
//! camera was unplugged, say, or an output clock thread that had panicked
//! and was joined) leaves nothing of the session that could call CUDA: its
//! elements were stopped and dropped, the effect worker joined with them,
//! and the control is a V4L2 ioctl. It is warned, and the GPU parked.
//!
//! [`Parker::stop_session`] and [`Parker::start_session`] are the session
//! lifecycle around it, over any [`StopSession`]: the GStreamer backend's
//! order, unit-tested here with a recording session.
//!
//! **System memory.** A session whose frames stay in system memory makes
//! no CUDA call, so it is neither unparked nor parked
//! ([`Parker::start_session_in`], [`StopSession::uses_cuda`]): it starts
//! while the GPU is parked, leaves the GPU as it found it (parked, or awake
//! after a park that failed), and never taints the parker (nothing of it
//! could call CUDA). A CUDA session it replaces is stopped and parked first,
//! as at any stop.

use std::time::Duration;

use obc_core::config::VideoMemory;
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
    /// cleanly: once this returns, nothing of it uses CUDA. An error says
    /// whether that holds all the same ([`StopFailure::reached_null`]).
    fn stop(self) -> Result<(), StopFailure>;

    /// Whether the session's frames are in CUDA memory (the default): a
    /// session in system memory made no CUDA call, and its stop parks
    /// nothing.
    fn uses_cuda(&self) -> bool {
        true
    }
}

/// How a session stop failed ([`StopSession::stop`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopFailure {
    /// What failed, as the parker warns it.
    pub error: String,
    /// Every pipeline of the session reached NULL all the same, those its
    /// camera switches took down included: nothing of it is left that could
    /// call CUDA, and the GPU is parked. `false` (a pipeline left outside
    /// NULL, or not confirmed in it): something of it may still be alive
    /// and call CUDA, which taints the parker.
    pub reached_null: bool,
}

impl StopFailure {
    /// A stop that left a pipeline outside NULL, because of `error`.
    pub fn unclean(error: impl Into<String>) -> Self {
        StopFailure {
            error: error.into(),
            reached_null: false,
        }
    }

    /// A stop that failed with `error`, every pipeline in NULL all the
    /// same.
    pub fn at_null(error: impl Into<String>) -> Self {
        StopFailure {
            error: error.into(),
            reached_null: true,
        }
    }
}

/// Why a session start failed, as [`Parker::starting`] sees it: the error,
/// whether what the start had built was cleaned up cleanly, and whether it
/// failed for want of CUDA. A `String` converts into a clean one that is
/// not for want of CUDA: a start that failed before it built anything that
/// could use CUDA, or whose partial build stopped cleanly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartError {
    /// The start's error, as the caller reports it.
    pub error: String,
    /// `Some(why)` when what the start had built did not stop cleanly when
    /// it was cleaned up (a pipeline left short of NULL, say): something of
    /// it may still be alive and call CUDA.
    pub unclean: Option<String>,
    /// The start failed for want of CUDA (no usable device, the `nvcodec`
    /// elements missing, no CUDA context opened): the process holds no CUDA
    /// context or memory, so a park has nothing to release, and none is
    /// made. The driver's checkpoint lock is not safe there: with
    /// `CUDA_VISIBLE_DEVICES` naming no GPU, it crashed inside `libcuda`.
    pub no_cuda: bool,
}

impl StartError {
    /// A start that failed with `error`, and whose partial build did not
    /// stop cleanly, because of `why`.
    pub fn unclean(error: impl Into<String>, why: impl Into<String>) -> Self {
        StartError {
            error: error.into(),
            unclean: Some(why.into()),
            no_cuda: false,
        }
    }

    /// The same error, marked as for want of CUDA ([`Self::no_cuda`]) when
    /// `no_cuda`.
    #[must_use]
    pub fn for_want_of_cuda(mut self, no_cuda: bool) -> Self {
        self.no_cuda = no_cuda;
        self
    }
}

impl From<String> for StartError {
    fn from(error: String) -> Self {
        StartError {
            error,
            unclean: None,
            no_cuda: false,
        }
    }
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
    /// A session did not stop cleanly: never parks again in this process
    /// (see the module docs).
    tainted: bool,
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
            .field("tainted", &self.tainted)
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
            tainted: false,
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

    /// Whether a session did not stop cleanly, so that parking is off until
    /// the process exits (see the module docs).
    pub fn tainted(&self) -> bool {
        self.tainted
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
    /// Does nothing when parking is off, unsupported, or tainted.
    ///
    /// **It blocks its caller** (the controller thread, D-Bus included)
    /// while the driver parks: measured 15 to 20 ms at Idle after a clean
    /// stop, when only CUDA's initialisation is left to checkpoint. The
    /// worst case is longer: when another thread is still inside a CUDA
    /// call, the lock waits up to [`obc_gpu::PARK_LOCK_TIMEOUT`] (2000 ms)
    /// before the park fails and the GPU stays awake, and a process holding
    /// GPU memory takes longer to checkpoint (about 120 ms for 2 MiB in a
    /// live context). A session that did not stop cleanly, the likely
    /// source of such a thread, taints the parker, so it is not parked at
    /// all.
    pub fn park(&mut self) {
        self.note_refusals();
        if !self.enabled || self.unsupported {
            return;
        }
        if self.tainted {
            (self.log)(
                Level::Debug,
                "GPU not parked: parking is disabled until the daemon restarts",
            );
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
    /// the error. If `start` fails, its [`StartError::error`] is the error,
    /// and the GPU is parked again, so a failed start does not keep it
    /// awake, unless what it had built did not stop cleanly
    /// ([`StartError::unclean`]): that taints the parker instead (see the
    /// module docs). A start that failed for want of CUDA
    /// ([`StartError::no_cuda`]) is not parked: no driver call is made.
    pub fn starting<T, E: Into<StartError>>(
        &mut self,
        start: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, String> {
        self.unpark()?;
        match start() {
            Ok(t) => Ok(t),
            Err(e) => {
                let StartError {
                    error,
                    unclean,
                    no_cuda,
                } = e.into();
                match unclean {
                    Some(why) => self.taint(&format!(
                        "the session start failed ({error}), and cleaning up what it had built \
                         did not stop cleanly: {why}"
                    )),
                    None if no_cuda => (self.log)(
                        Level::Debug,
                        "GPU not parked: the session start failed for want of CUDA, so the \
                         daemon holds no CUDA context to park",
                    ),
                    None => self.park(),
                }
                Err(error)
            }
        }
    }

    /// Stops the session in `running`, if any, all of it, and parks the
    /// GPU only then, and only if every pipeline of it reached NULL: a
    /// session that left one outside NULL may still use CUDA, which would
    /// block once parked, so it taints the parker instead (see the module
    /// docs). A stop that failed otherwise is warned, and parks.
    pub fn stop_session<S: StopSession>(&mut self, running: &mut Option<S>) {
        let Some(session) = running.take() else {
            return;
        };
        if !session.uses_cuda() {
            // Nothing of it could call CUDA: whatever its stop left, no
            // park is needed and parking is not at risk.
            if let Err(StopFailure { error, .. }) = session.stop() {
                (self.log)(
                    Level::Warning,
                    &format!("stopping the session (system memory): {error}"),
                );
            }
            return;
        }
        match session.stop() {
            Ok(()) => self.park(),
            Err(StopFailure {
                error,
                reached_null: true,
            }) => {
                (self.log)(
                    Level::Warning,
                    &format!("stopping the session: {error} (every pipeline reached NULL)"),
                );
                self.park();
            }
            Err(StopFailure {
                error,
                reached_null: false,
            }) => self.taint(&format!("stopping the session: {error}")),
        }
    }

    /// Starts a session in `running` with `build`, which touches CUDA: a
    /// session running is stopped first, without parking in between (the
    /// GPU is in use again at once; a stop that left a pipeline outside
    /// NULL taints the parker, one that failed otherwise is warned), then
    /// [`Self::starting`] unparks the GPU, builds, and parks it again if
    /// the build fails and the parker is not tainted. On error `running` is
    /// empty.
    pub fn start_session<S: StopSession, E: Into<StartError>>(
        &mut self,
        running: &mut Option<S>,
        build: impl FnOnce() -> Result<S, E>,
    ) -> Result<(), String> {
        if let Some(session) = running.take() {
            (self.log)(Level::Debug, "session restarted: not parking in between");
            match session.stop() {
                Ok(()) => {}
                Err(StopFailure {
                    error,
                    reached_null: true,
                }) => (self.log)(
                    Level::Warning,
                    &format!(
                        "stopping the session before a restart: {error} (every pipeline \
                         reached NULL)"
                    ),
                ),
                Err(StopFailure {
                    error,
                    reached_null: false,
                }) => self.taint(&format!("stopping the session before a restart: {error}")),
            }
        }
        *running = Some(self.starting(build)?);
        Ok(())
    }

    /// [`Self::start_session`] for a session in `memory`. In
    /// [`VideoMemory::Cuda`] it is [`Self::start_session`]. In
    /// [`VideoMemory::System`] the session makes no CUDA call: a session
    /// running is stopped as [`Self::stop_session`] stops it (a CUDA one is
    /// parked), then `build` runs with no unpark, and a build that fails is
    /// not parked after, nor does an unclean cleanup of it taint the
    /// parker (it is warned). On error `running` is empty.
    pub fn start_session_in<S: StopSession, E: Into<StartError>>(
        &mut self,
        running: &mut Option<S>,
        memory: VideoMemory,
        build: impl FnOnce() -> Result<S, E>,
    ) -> Result<(), String> {
        if memory == VideoMemory::Cuda {
            return self.start_session(running, build);
        }
        self.stop_session(running);
        match build() {
            Ok(s) => {
                *running = Some(s);
                Ok(())
            }
            Err(e) => {
                let StartError { error, unclean, .. } = e.into();
                if let Some(why) = unclean {
                    (self.log)(
                        Level::Warning,
                        &format!(
                            "the session start failed ({error}), and cleaning up what it had \
                             built did not stop cleanly: {why}"
                        ),
                    );
                }
                Err(error)
            }
        }
    }

    /// A teardown was unclean (`what` says which, and why): parking is off
    /// for the rest of the process. Warned: the first one, while parking
    /// was possible, says that it is now disabled; a later one, or one
    /// while parking was off or unsupported anyway, says only what failed
    /// (and, while off, that turning it on again will not park).
    fn taint(&mut self, what: &str) {
        let first = !self.tainted;
        self.tainted = true;
        let msg = if !first {
            what.to_string()
        } else if self.unsupported {
            format!("{what} (something of the session may still use the GPU)")
        } else if !self.enabled {
            format!(
                "{what} (something of the session may still use the GPU); parking is off, and \
                 stays off until the daemon restarts even if turned on again"
            )
        } else {
            format!(
                "{what}; parking the GPU is disabled until the daemon restarts, because a \
                 session did not stop cleanly (something of it may still use the GPU): it stays \
                 awake while idle"
            )
        };
        (self.log)(Level::Warning, &msg);
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

    /// A recording session: its stop is an event, and fails with `fail`;
    /// in system memory with `host`.
    struct Rec {
        ops: ScriptOps,
        fail: Option<StopFailure>,
        host: bool,
    }

    impl StopSession for Rec {
        fn stop(self) -> Result<(), StopFailure> {
            self.ops.event("stop");
            self.fail.map_or(Ok(()), Err)
        }

        fn uses_cuda(&self) -> bool {
            !self.host
        }
    }

    fn rec(ops: &ScriptOps) -> Rec {
        Rec {
            ops: ops.clone(),
            fail: None,
            host: false,
        }
    }

    fn host_rec(ops: &ScriptOps) -> Rec {
        Rec {
            host: true,
            ..rec(ops)
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
        assert_eq!(p.starting(|| Ok::<_, String>(7)), Ok(7));
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
                Ok::<_, String>(())
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

    /// A session whose stop leaves a pipeline outside NULL, with `why`.
    fn failing(ops: &ScriptOps, why: &str) -> Rec {
        Rec {
            ops: ops.clone(),
            fail: Some(StopFailure::unclean(why)),
            host: false,
        }
    }

    /// A session whose stop fails with `why`, every pipeline in NULL.
    fn failing_at_null(ops: &ScriptOps, why: &str) -> Rec {
        Rec {
            ops: ops.clone(),
            fail: Some(StopFailure::at_null(why)),
            host: false,
        }
    }

    /// The one warning that turns parking off, naming `cause`.
    fn assert_disabled_warning(l: &[(Level, String)], cause: &str) {
        assert_eq!(l.len(), 1, "{l:?}");
        assert_eq!(l[0].0, Level::Warning);
        assert!(
            l[0].1.contains(cause)
                && l[0]
                    .1
                    .contains("parking the GPU is disabled until the daemon restarts")
                && l[0].1.contains("did not stop cleanly")
                && l[0].1.contains("stays awake"),
            "{}",
            l[0].1
        );
    }

    #[test]
    fn a_session_that_did_not_stop_cleanly_disables_parking_for_good() {
        let (mut p, ops, logged) = parker();
        let mut running = Some(failing(&ops, "output pipeline ended in Paused, not Null"));
        p.stop_session(&mut running);
        assert!(running.is_none());
        assert_eq!(ops.events(), ["stop"], "not parked");
        assert!(p.tainted());
        assert_disabled_warning(&take(&logged), "ended in Paused, not Null");
        // Not parked: the next start finds it running and builds.
        ops.0
            .lock()
            .unwrap()
            .unpark
            .push_back(Ok(UnparkOutcome::NotParked));
        let o = ops.clone();
        p.start_session(&mut running, || {
            o.event("build");
            Ok::<_, String>(rec(&o))
        })
        .unwrap();
        assert_eq!(ops.events(), ["unpark", "build"]);
        // A later clean stop does not prove the leftovers gone: still no
        // park, and nothing more said at warning.
        p.stop_session(&mut running);
        assert_eq!(ops.events(), ["stop"], "sticky: not parked");
        assert!(take(&logged).iter().all(|(lv, _)| *lv == Level::Debug));
        // A second unclean stop says what failed, not the whole story again.
        running = Some(failing(&ops, "ingest ended in Ready, not Null"));
        p.stop_session(&mut running);
        let l = take(&logged);
        assert_eq!(l.len(), 1, "{l:?}");
        assert_eq!(l[0].0, Level::Warning);
        assert!(l[0].1.contains("ended in Ready"), "{}", l[0].1);
        assert!(!l[0].1.contains("disabled"), "said once: {}", l[0].1);
        assert_eq!(ops.0.lock().unwrap().parks, 0);
    }

    #[test]
    fn a_restart_whose_stop_failed_disables_parking_even_if_the_build_fails() {
        let (mut p, ops, logged) = parker();
        ops.0
            .lock()
            .unwrap()
            .unpark
            .push_back(Ok(UnparkOutcome::NotParked));
        let mut running = Some(failing(&ops, "output pipeline ended in Paused, not Null"));
        let o = ops.clone();
        let e = p
            .start_session(&mut running, || -> Result<Rec, String> {
                o.event("build");
                Err("output pipeline: no cudaupload".into())
            })
            .unwrap_err();
        assert_eq!(e, "output pipeline: no cudaupload");
        assert!(running.is_none());
        assert_eq!(ops.events(), ["stop", "unpark", "build"], "not parked");
        assert!(p.tainted());
        let l: Vec<_> = take(&logged)
            .into_iter()
            .filter(|(lv, _)| *lv != Level::Debug)
            .collect();
        assert_disabled_warning(&l, "before a restart: output pipeline ended in Paused");
    }

    #[test]
    fn a_failed_start_whose_cleanup_was_unclean_disables_parking() {
        let (mut p, ops, logged) = parker();
        {
            let mut s = ops.0.lock().unwrap();
            s.unpark.push_back(Ok(UNPARKED));
            s.unpark.push_back(Ok(UnparkOutcome::NotParked));
        }
        let mut running: Option<Rec> = None;
        let o = ops.clone();
        let e = p
            .start_session(&mut running, || -> Result<Rec, StartError> {
                o.event("build");
                Err(StartError::unclean(
                    "output pipeline: state change failed",
                    "output pipeline: ended in Paused, not Null",
                ))
            })
            .unwrap_err();
        // The caller reports the start's error, not the cleanup's.
        assert_eq!(e, "output pipeline: state change failed");
        assert!(running.is_none());
        assert_eq!(ops.events(), ["unpark", "build"], "not parked again");
        assert!(p.tainted());
        let l: Vec<_> = take(&logged)
            .into_iter()
            .filter(|(lv, _)| *lv == Level::Warning)
            .collect();
        assert_disabled_warning(&l, "cleaning up what it had built");
        assert!(l[0].1.contains("ended in Paused, not Null"), "{}", l[0].1);
        // Sticky: a later clean failed start parks nothing either.
        let e = p
            .starting(|| -> Result<(), StartError> { Err("no cudaupload".to_string().into()) })
            .unwrap_err();
        assert_eq!(e, "no cudaupload");
        assert_eq!(ops.0.lock().unwrap().parks, 0);
    }

    #[test]
    fn a_failed_start_that_cleaned_up_cleanly_parks_again() {
        let (mut p, ops, _logged) = parker();
        {
            let mut s = ops.0.lock().unwrap();
            s.unpark.push_back(Ok(UNPARKED));
            s.park.push_back(Ok(PARKED));
        }
        let e = p
            .starting(|| -> Result<(), StartError> {
                Err(StartError::from(
                    "output pipeline: no cudaupload".to_string(),
                ))
            })
            .unwrap_err();
        assert_eq!(e, "output pipeline: no cudaupload");
        assert_eq!(ops.events(), ["unpark", "park"]);
        assert!(!p.tainted());
    }

    #[test]
    fn a_start_that_failed_for_want_of_cuda_makes_no_park_call() {
        let (mut p, ops, logged) = parker();
        ops.0
            .lock()
            .unwrap()
            .unpark
            .push_back(Ok(UnparkOutcome::NotParked));
        let mut running: Option<Rec> = None;
        let o = ops.clone();
        let e = p
            .start_session(&mut running, || -> Result<Rec, StartError> {
                o.event("build");
                Err(
                    StartError::from("output pipeline: no cudaupload".to_string())
                        .for_want_of_cuda(true),
                )
            })
            .unwrap_err();
        assert_eq!(e, "output pipeline: no cudaupload");
        assert!(running.is_none());
        // No park is asked of the driver (an unscripted one would panic).
        assert_eq!(ops.events(), ["unpark", "build"]);
        assert_eq!(ops.0.lock().unwrap().parks, 0);
        assert!(!p.tainted());
        let l = take(&logged);
        assert!(l.iter().all(|(lv, _)| *lv == Level::Debug), "{l:?}");
        // An unclean cleanup of such a start still taints, with no park
        // either.
        ops.0
            .lock()
            .unwrap()
            .unpark
            .push_back(Ok(UnparkOutcome::NotParked));
        let e = p
            .starting(|| -> Result<(), StartError> {
                Err(
                    StartError::unclean("no CUDA context", "upload stuck in READY")
                        .for_want_of_cuda(true),
                )
            })
            .unwrap_err();
        assert_eq!(e, "no CUDA context");
        assert!(p.tainted());
        assert_eq!(ops.0.lock().unwrap().parks, 0);
    }

    #[test]
    fn a_tainted_parker_still_unparks_a_parked_process() {
        let (mut p, ops, _logged) = parker();
        {
            let mut s = ops.0.lock().unwrap();
            s.park.push_back(Ok(PARKED));
            s.unpark.push_back(Ok(UNPARKED));
        }
        p.park();
        // Tainted while parked (a session the restart replaces failed its
        // stop): the start restores the GPU as usual.
        let mut running = Some(failing(&ops, "ended in Paused, not Null"));
        let o = ops.clone();
        p.start_session(&mut running, || {
            o.event("build");
            Ok::<_, String>(rec(&o))
        })
        .unwrap();
        assert!(p.tainted());
        assert_eq!(ops.events(), ["park", "stop", "unpark", "build"]);
        assert_eq!(p.last_unpark(), Some(Duration::from_millis(62)));
    }

    #[test]
    fn a_stop_that_failed_with_every_pipeline_at_null_is_warned_and_parks() {
        // A camera control not restored (the camera was unplugged), or an
        // output clock that had panicked: nothing of the session is left.
        let (mut p, ops, logged) = parker();
        ops.0.lock().unwrap().park.push_back(Ok(PARKED));
        let mut running = Some(failing_at_null(
            &ops,
            "camera control on /dev/video0: restoring exposure_dynamic_framerate: No such device",
        ));
        p.stop_session(&mut running);
        assert!(running.is_none());
        assert_eq!(ops.events(), ["stop", "park"]);
        assert!(!p.tainted());
        let l = take(&logged);
        assert_eq!(l[0].0, Level::Warning, "{l:?}");
        assert!(
            l[0].1.starts_with("stopping the session: camera control")
                && l[0].1.contains("every pipeline reached NULL"),
            "{}",
            l[0].1
        );
        assert!(!l[0].1.contains("disabled"), "{}", l[0].1);
        assert!(l[1].1.starts_with("GPU parked in"), "{l:?}");
    }

    #[test]
    fn a_restart_whose_stop_failed_at_null_is_warned_and_parks_again_if_the_build_fails() {
        let (mut p, ops, logged) = parker();
        {
            let mut s = ops.0.lock().unwrap();
            s.unpark.push_back(Ok(UnparkOutcome::NotParked));
            s.park.push_back(Ok(PARKED));
        }
        let mut running = Some(failing_at_null(
            &ops,
            "the output clock thread panicked: boom",
        ));
        let o = ops.clone();
        let e = p
            .start_session(&mut running, || -> Result<Rec, String> {
                o.event("build");
                Err("output pipeline: no cudaupload".into())
            })
            .unwrap_err();
        assert_eq!(e, "output pipeline: no cudaupload");
        assert_eq!(ops.events(), ["stop", "unpark", "build", "park"]);
        assert!(!p.tainted());
        let warned: Vec<_> = take(&logged)
            .into_iter()
            .filter(|(lv, _)| *lv == Level::Warning)
            .collect();
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert!(
            warned[0]
                .1
                .starts_with("stopping the session before a restart: the output clock")
                && warned[0].1.contains("every pipeline reached NULL"),
            "{}",
            warned[0].1
        );
    }

    #[test]
    fn a_taint_while_parking_is_unsupported_or_off_does_not_say_it_disables_parking() {
        // Unsupported: parking was never possible here.
        let (mut p, ops, logged) = parker();
        ops.0
            .lock()
            .unwrap()
            .unpark
            .push_back(Ok(UnparkOutcome::Unsupported("no checkpoint API".into())));
        assert_eq!(p.unpark(), Ok(()));
        take(&logged);
        let mut running = Some(failing(&ops, "ended in Paused, not Null"));
        p.stop_session(&mut running);
        assert!(p.tainted());
        let l = take(&logged);
        assert_eq!(l.len(), 1, "{l:?}");
        assert_eq!(l[0].0, Level::Warning);
        assert!(l[0].1.contains("ended in Paused, not Null"), "{}", l[0].1);
        assert!(
            !l[0].1.contains("disabled") && !l[0].1.contains("parking"),
            "{}",
            l[0].1
        );

        // Off: it stays off, even turned on again.
        let (mut p, ops, logged) = parker();
        p.set_enabled(false);
        let mut running = Some(failing(&ops, "ended in Paused, not Null"));
        p.stop_session(&mut running);
        assert!(p.tainted());
        let l = take(&logged);
        assert_eq!(l.len(), 1, "{l:?}");
        assert!(
            !l[0].1.contains("parking the GPU is disabled")
                && l[0]
                    .1
                    .contains("parking is off, and stays off until the daemon restarts"),
            "{}",
            l[0].1
        );
        p.set_enabled(true);
        running = Some(rec(&ops));
        p.stop_session(&mut running);
        assert_eq!(ops.events(), ["stop", "stop"], "never parked");
        assert_eq!(ops.0.lock().unwrap().parks, 0);
    }

    #[test]
    fn a_start_unparks_before_building_and_parks_nothing_when_it_works() {
        let (mut p, ops, _logged) = parker();
        ops.0.lock().unwrap().unpark.push_back(Ok(UNPARKED));
        let mut running: Option<Rec> = None;
        let o = ops.clone();
        p.start_session(&mut running, || {
            o.event("build");
            Ok::<_, String>(rec(&o))
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
            Ok::<_, String>(rec(&o))
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

    #[test]
    fn a_session_in_system_memory_is_neither_unparked_nor_parked() {
        // No driver call is scripted: any would panic.
        let (mut p, ops, logged) = parker();
        let mut running: Option<Rec> = None;
        let o = ops.clone();
        p.start_session_in(
            &mut running,
            VideoMemory::System,
            || -> Result<Rec, String> {
                o.event("build");
                Ok(host_rec(&o))
            },
        )
        .unwrap();
        assert!(running.is_some());
        p.stop_session(&mut running);
        assert!(running.is_none());
        assert_eq!(ops.events(), ["build", "stop"]);
        let s = ops.0.lock().unwrap();
        assert_eq!((s.parks, s.unparks), (0, 0));
        drop(s);
        assert!(take(&logged).is_empty());
    }

    #[test]
    fn a_failed_start_in_system_memory_parks_nothing_and_never_taints() {
        let (mut p, ops, logged) = parker();
        let mut running: Option<Rec> = None;
        let e = p
            .start_session_in(
                &mut running,
                VideoMemory::System,
                || -> Result<Rec, StartError> {
                    Err(StartError::unclean(
                        "output pipeline: busy",
                        "convert stuck in PAUSED",
                    ))
                },
            )
            .unwrap_err();
        assert_eq!(e, "output pipeline: busy");
        assert!(running.is_none());
        assert!(
            !p.tainted(),
            "nothing of a system-memory session can call CUDA"
        );
        let l = take(&logged);
        assert_eq!(l.len(), 1, "{l:?}");
        assert_eq!(l[0].0, Level::Warning);
        // An unclean stop of a system-memory session is warned, not tainting.
        let mut running = Some(Rec {
            fail: Some(StopFailure::unclean("sink stuck")),
            ..host_rec(&ops)
        });
        p.stop_session(&mut running);
        assert!(!p.tainted());
        assert_eq!(ops.events(), ["stop"]);
        assert_eq!(take(&logged)[0].0, Level::Warning);
    }

    #[test]
    fn a_cuda_session_replaced_by_one_in_system_memory_is_parked_first() {
        let (mut p, ops, _logged) = parker();
        ops.0.lock().unwrap().park.push_back(Ok(PARKED));
        let mut running = Some(rec(&ops));
        let o = ops.clone();
        p.start_session_in(
            &mut running,
            VideoMemory::System,
            || -> Result<Rec, String> {
                o.event("build");
                Ok(host_rec(&o))
            },
        )
        .unwrap();
        assert_eq!(ops.events(), ["stop", "park", "build"]);
        assert_eq!(ops.0.lock().unwrap().unparks, 0);
    }

    #[test]
    fn a_start_in_cuda_memory_is_the_parked_lifecycle() {
        let (mut p, ops, _logged) = parker();
        ops.0.lock().unwrap().unpark.push_back(Ok(UNPARKED));
        let mut running: Option<Rec> = None;
        let o = ops.clone();
        p.start_session_in(
            &mut running,
            VideoMemory::Cuda,
            || -> Result<Rec, String> {
                o.event("build");
                Ok(rec(&o))
            },
        )
        .unwrap();
        assert_eq!(ops.events(), ["unpark", "build"]);
    }
}
