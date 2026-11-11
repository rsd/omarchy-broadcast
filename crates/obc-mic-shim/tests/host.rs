//! A LADSPA test host for the forwarding plugin: it goes through
//! `ladspa_descriptor` and the descriptor's function pointers exactly as
//! filter-chain does, against an in-test fake worker built on the
//! worker-side API of `obc_audio::server`.
//!
//! Every `run()` call is made with a counting allocator armed on the
//! calling thread; any allocation or free inside `run()` fails the test.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::VecDeque;
use std::f64::consts::PI;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use obc_audio::proto::{MAX_MESSAGE_LEN, Reply};
use obc_audio::server::{Handshake, Listener, Wait};
use obc_audio::shm::{Ring, SLOT_FLAG_RAW};
use obc_audio::{BLOCK_SAMPLES, transport_delay};
use omarchy_broadcast_mic::ladspa::{self, Descriptor, LadspaHandle, port};
use omarchy_broadcast_mic::{ShimConfig, ShimStats, configure, ladspa_descriptor, stats};

// ---------------------------------------------------------------- allocator

struct Counting;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

fn note() {
    let _ = ARMED.try_with(|a| {
        if a.get() {
            let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        }
    });
}

// SAFETY: forwards to the system allocator; the bookkeeping touches only
// const-initialised thread-locals, which never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        note();
        // SAFETY: the caller's contract, forwarded.
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        note();
        // SAFETY: the caller's contract, forwarded.
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        note();
        // SAFETY: the caller's contract, forwarded.
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        note();
        // SAFETY: the caller's contract, forwarded.
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

// ---------------------------------------------------------------- signals

const MAX_RUN: usize = 4096;
const SR: f64 = 48_000.0;

/// A 440 Hz sine at half scale: smooth, so any output step stands out.
fn sine(t: i64) -> f32 {
    if t < 0 {
        return 0.0;
    }
    (0.5 * (2.0 * PI * 440.0 * t as f64 / SR).sin()) as f32
}

/// Largest sample-to-sample change of the sine, plus crossfade slack.
const MAX_STEP: f32 = 0.04;

/// Deterministic pseudo-random block sizes in 32..=2048.
struct Lcg(u64);
impl Lcg {
    fn size(&mut self) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        32 + ((self.0 >> 33) % 2017) as usize
    }
}

// ---------------------------------------------------------------- host

struct Host {
    desc: &'static Descriptor,
    h: LadspaHandle,
    inp: Box<[f32; MAX_RUN]>,
    out: Box<[f32; MAX_RUN]>,
    latency: Box<f32>,
    bypass: Box<f32>,
    /// Samples run so far.
    t: usize,
    /// Every output sample.
    output: Vec<f32>,
    /// The latency port after every run.
    latencies: Vec<f32>,
}

impl Host {
    fn new(config: ShimConfig) -> Self {
        let d = ladspa_descriptor(0);
        assert!(!d.is_null());
        // SAFETY: the plugin's static descriptor.
        let desc: &'static Descriptor = unsafe { &*d };
        // SAFETY: calling the plugin as a LADSPA host would.
        let h = unsafe { (desc.instantiate.unwrap())(desc, 48_000) };
        assert!(!h.is_null());
        // SAFETY: a live handle, nothing else running on it.
        unsafe { configure(h, config) };
        let mut host = Self {
            desc,
            h,
            inp: Box::new([0.0; MAX_RUN]),
            out: Box::new([0.0; MAX_RUN]),
            latency: Box::new(-1.0),
            bypass: Box::new(0.0),
            t: 0,
            output: Vec::with_capacity(1 << 20),
            latencies: Vec::with_capacity(1 << 14),
        };
        // SAFETY: the buffers are boxed and outlive the handle.
        unsafe {
            let cp = desc.connect_port.unwrap();
            cp(h, port::INPUT as _, host.inp.as_mut_ptr());
            cp(h, port::OUTPUT as _, host.out.as_mut_ptr());
            cp(h, port::LATENCY as _, &mut *host.latency);
            cp(h, port::BYPASS as _, &mut *host.bypass);
            (desc.activate.unwrap())(h);
        }
        host
    }

    /// Runs `n` samples of `signal`, with the allocator armed around `run()`.
    fn run_with(&mut self, n: usize, signal: impl Fn(i64) -> f32) {
        assert!(n <= MAX_RUN);
        for i in 0..n {
            self.inp[i] = signal((self.t + i) as i64);
        }
        let run = self.desc.run.unwrap();
        ARMED.with(|a| a.set(true));
        // SAFETY: a live, activated handle with all ports connected.
        unsafe { run(self.h, n as _) };
        ARMED.with(|a| a.set(false));
        let allocs = ALLOCS.with(|c| c.replace(0));
        assert_eq!(allocs, 0, "run() allocated or freed {allocs} times");
        self.output.extend_from_slice(&self.out[..n]);
        self.latencies.push(*self.latency);
        self.t += n;
    }

    fn run(&mut self, n: usize) {
        self.run_with(n, sine);
    }

    fn stats(&self) -> ShimStats {
        // SAFETY: a live handle, not running.
        unsafe { stats(self.h) }
    }

    /// Runs quanta of `q` until `cond` holds, giving the helper thread time.
    fn run_until(
        &mut self,
        q: usize,
        worker: Option<&FakeWorker>,
        what: &str,
        cond: impl Fn(&Self) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cond(self) {
            assert!(Instant::now() < deadline, "timed out waiting for: {what}");
            self.run(q);
            if let Some(w) = worker {
                w.sync(self);
            }
            thread::sleep(Duration::from_micros(500));
        }
    }

    /// Runs `count` quanta of `q`, syncing with `worker` after each.
    fn pump(&mut self, q: usize, count: usize, worker: Option<&FakeWorker>) {
        for _ in 0..count {
            self.run(q);
            if let Some(w) = worker {
                w.sync(self);
            }
        }
    }

    fn max_step(&self, from: usize) -> (usize, f32) {
        let mut worst = (0, 0.0f32);
        for i in from.max(1)..self.output.len() {
            let d = (self.output[i] - self.output[i - 1]).abs();
            if d > worst.1 {
                worst = (i, d);
            }
        }
        worst
    }

    /// Asserts `output[t] == expected(t)` bit-exactly over `range`.
    fn assert_exact(
        &self,
        range: std::ops::Range<usize>,
        what: &str,
        expected: impl Fn(i64) -> f32,
    ) {
        for t in range {
            let e = expected(t as i64);
            assert_eq!(
                self.output[t].to_bits(),
                e.to_bits(),
                "{what}: sample {t}: got {} expected {e}",
                self.output[t]
            );
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        // SAFETY: the handle is live; LADSPA's teardown order.
        unsafe {
            (self.desc.deactivate.unwrap())(self.h);
            (self.desc.cleanup.unwrap())(self.h);
        }
    }
}

fn socket_path() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "obc-shim-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("audio.sock")
}

fn test_config(path: &Path) -> ShimConfig {
    ShimConfig {
        socket_path: Some(path.to_owned()),
        connect_timeout: Duration::from_millis(500),
        retry_min: Duration::from_millis(10),
        retry_max: Duration::from_millis(50),
        reconnect_delay: Duration::from_millis(20),
        hung_timeout: Duration::from_millis(300),
        ..ShimConfig::default()
    }
}

// ---------------------------------------------------------------- fake worker

#[derive(Clone, Copy)]
enum Mode {
    /// Multiply by `gain` after an algorithmic delay of `latency` samples.
    Gain { gain: f32, latency: u32, depth: u32 },
    /// Answer every hello with a reply of another protocol version.
    OtherVersion,
    /// Accept, then never beat and never answer.
    Stall,
    /// Accept, answer `good` blocks per connection like `GAIN`, then
    /// misbehave as `bad` says (while still beating).
    Bad { bad: Bad, good: u64 },
}

/// Ways a worker can misbehave.
#[derive(Clone, Copy, Debug)]
enum Bad {
    /// Publish an output head far ahead of what it wrote.
    OutHeadAhead,
    /// Publish an input tail ahead of what the plugin sent.
    InTailAhead,
    /// Answer with sequence numbers in the future and in the past.
    WrongSeq,
    /// Answer with a slot `n_samples` other than 480.
    WrongN,
    /// Publish an absurd algorithmic delay (answers stay correct).
    HugeLatency,
}

#[derive(Default)]
struct WorkerState {
    hellos: AtomicU64,
    /// Input blocks taken (answered or deliberately skipped).
    handled: AtomicU64,
    /// Sequence numbers not to answer.
    skip: Mutex<Vec<u64>>,
    last_seq: AtomicU64,
    /// Connections that ended with the plugin hanging up.
    hangups: AtomicU64,
    /// The transport delay the plugin published last (0: none).
    transport: AtomicU64,
}

struct FakeWorker {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    st: Arc<WorkerState>,
    /// The plugin's `blocks_sent` when this worker started.
    base: u64,
}

impl FakeWorker {
    fn start(path: &Path, mode: Mode, host: &Host) -> Self {
        let _ = std::fs::remove_file(path);
        let listener = Listener::bind(path).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let st = Arc::new(WorkerState::default());
        let (stop2, st2) = (Arc::clone(&stop), Arc::clone(&st));
        let thread = thread::spawn(move || worker_main(listener, mode, &stop2, &st2));
        Self {
            path: path.to_owned(),
            stop,
            thread: Some(thread),
            st,
            base: host.stats().blocks_sent,
        }
    }

    fn hellos(&self) -> u64 {
        self.st.hellos.load(Ordering::SeqCst)
    }

    fn skip(&self, seq: u64) {
        self.st.skip.lock().unwrap().push(seq);
    }

    /// Waits until every block the plugin has sent since this worker
    /// started has been taken: the worker is "always on time".
    fn sync(&self, host: &Host) {
        let target = host.stats().blocks_sent;
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.base + self.st.handled.load(Ordering::SeqCst) < target {
            assert!(Instant::now() < deadline, "fake worker fell behind");
            thread::yield_now();
        }
    }

    /// Kills the worker: its connection closes and the socket goes away.
    fn kill(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for FakeWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn worker_main(listener: Listener, mode: Mode, stop: &AtomicBool, st: &WorkerState) {
    while !stop.load(Ordering::SeqCst) {
        let Some(conn) = listener.accept_timeout(Duration::from_millis(5)).unwrap() else {
            continue;
        };
        st.hellos.fetch_add(1, Ordering::SeqCst);
        match mode {
            Mode::OtherVersion => {
                let mut buf = [0u8; MAX_MESSAGE_LEN];
                let _ = conn.recv(&mut buf);
                let mut wire = Reply::encode_accept(1, 0);
                wire[4..8].copy_from_slice(&(obc_audio::PROTOCOL_VERSION + 1).to_le_bytes());
                let _ = conn.send(&wire, &[]);
            }
            Mode::Bad { bad, good } => bad_worker(conn, bad, good, stop, st),
            Mode::Stall => {
                let hs = Handshake::read(conn, Duration::from_secs(1)).unwrap();
                let ep = hs.accept(1, 0).unwrap();
                while !stop.load(Ordering::SeqCst) {
                    if ep.wait(Duration::from_millis(5)).unwrap() == Wait::HangUp {
                        break;
                    }
                }
            }
            Mode::Gain {
                gain,
                latency,
                depth,
            } => {
                let hs = Handshake::read(conn, Duration::from_secs(1)).unwrap();
                let mut ep = hs.accept(depth, latency).unwrap();
                let mut delay: VecDeque<f32> = std::iter::repeat_n(0.0, latency as usize).collect();
                let mut block = [0.0f32; BLOCK_SAMPLES];
                // Output samples produced since the stream started: the
                // first `latency` of them are the delay line's warm-up
                // zeros, so blocks holding any are flagged raw, as the
                // worker contract asks.
                let mut produced = 0u64;
                while !stop.load(Ordering::SeqCst) {
                    if ep.wait(Duration::from_millis(2)).unwrap() == Wait::HangUp {
                        st.hangups.fetch_add(1, Ordering::SeqCst);
                        break;
                    }
                    ep.beat();
                    while let Some(meta) = ep.pop_input(&mut block).unwrap() {
                        for s in &mut block {
                            delay.push_back(*s);
                            *s = gain * delay.pop_front().unwrap();
                        }
                        let flags = if produced < u64::from(latency) {
                            SLOT_FLAG_RAW
                        } else {
                            0
                        };
                        produced += BLOCK_SAMPLES as u64;
                        st.last_seq.store(meta.seq, Ordering::SeqCst);
                        st.transport.store(
                            u64::from(ep.transport_delay().unwrap_or(0)),
                            Ordering::SeqCst,
                        );
                        if !st.skip.lock().unwrap().contains(&meta.seq) {
                            assert!(ep.push_output(meta.seq, flags, &block));
                        }
                        st.handled.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
        }
    }
}

/// A worker that answers `good` blocks per connection with half the
/// level, then misbehaves. It pushes through the raw region API with its
/// own head counter, so it can write any slot header.
fn bad_worker(
    conn: obc_audio::sys::SeqPacket,
    bad: Bad,
    good: u64,
    stop: &AtomicBool,
    st: &WorkerState,
) {
    let hs = Handshake::read(conn, Duration::from_secs(1)).unwrap();
    let mut ep = hs.accept(1, 0).unwrap();
    let mut head = 0u64;
    let mut block = [0.0f32; BLOCK_SAMPLES];
    let mut answered = 0u64;
    while !stop.load(Ordering::SeqCst) {
        if ep.wait(Duration::from_millis(2)).unwrap() == Wait::HangUp {
            st.hangups.fetch_add(1, Ordering::SeqCst);
            break;
        }
        ep.beat();
        let misbehave = answered >= good;
        if misbehave {
            match bad {
                Bad::OutHeadAhead => ep.region().publish_counter(Ring::Output, true, head + 1000),
                Bad::InTailAhead => ep.region().publish_counter(Ring::Input, false, 1 << 40),
                Bad::HugeLatency => ep.region().set_latency_samples(u32::MAX),
                Bad::WrongSeq | Bad::WrongN => {}
            }
        }
        // With corrupt counters published, stop reading and writing.
        if misbehave && matches!(bad, Bad::InTailAhead | Bad::OutHeadAhead) {
            continue;
        }
        while let Some(meta) = ep.pop_input(&mut block).unwrap() {
            for s in &mut block {
                *s *= 0.5;
            }
            let (seq, n) = match (misbehave, bad) {
                (true, Bad::WrongSeq) if meta.seq % 2 == 0 => (meta.seq + 1000, 480),
                (true, Bad::WrongSeq) => (meta.seq.wrapping_sub(1000), 480),
                (true, Bad::WrongN) => (meta.seq, [0, 479, 481, u32::MAX][(meta.seq % 4) as usize]),
                _ => (meta.seq, BLOCK_SAMPLES as u32),
            };
            let _ = ep
                .region()
                .push_raw(Ring::Output, &mut head, seq, 0, n, &block);
            answered += 1;
            st.handled.fetch_add(1, Ordering::SeqCst);
        }
    }
}

const GAIN: Mode = Mode::Gain {
    gain: 0.5,
    latency: 0,
    depth: 1,
};

/// Connects a host to a fresh gain worker and runs until processing is on.
fn connected(q: usize, mode: Mode) -> (Host, FakeWorker, PathBuf) {
    let path = socket_path();
    let mut host = Host::new(test_config(&path));
    let worker = FakeWorker::start(&path, mode, &host);
    host.run_until(q, Some(&worker), "processing", |h| h.stats().processing);
    (host, worker, path)
}

// ---------------------------------------------------------------- tests

#[test]
fn descriptor_shape() {
    let d = ladspa_descriptor(0);
    assert!(ladspa_descriptor(1).is_null());
    // SAFETY: the plugin's static descriptor and its static arrays.
    unsafe {
        let d = &*d;
        assert_eq!(std::ffi::CStr::from_ptr(d.label), ladspa::LABEL);
        assert_eq!(d.unique_id, ladspa::UNIQUE_ID);
        assert_ne!(d.properties & ladspa::PROPERTY_HARD_RT_CAPABLE, 0);
        assert_eq!(d.port_count, 4);
        let names: Vec<_> = (0..4)
            .map(|i| {
                std::ffi::CStr::from_ptr(*d.port_names.add(i))
                    .to_str()
                    .unwrap()
            })
            .collect();
        assert_eq!(names, ["In", "Out", "latency", "bypass"]);
        let pd = std::slice::from_raw_parts(d.port_descriptors, 4);
        assert_eq!(
            pd[port::LATENCY],
            ladspa::PORT_CONTROL | ladspa::PORT_OUTPUT
        );
        assert_eq!(pd[port::BYPASS], ladspa::PORT_CONTROL | ladspa::PORT_INPUT);
        assert!(d.run_adding.is_none());
    }
}

#[test]
fn passthrough_without_worker_is_bit_exact_at_a_constant_delay() {
    let path = socket_path(); // nothing listens there
    let mut host = Host::new(test_config(&path));
    let noise = |t: i64| ((t.wrapping_mul(2_654_435_761) % 2001) as f32 / 1000.0) - 1.0;
    let mut lcg = Lcg(7);
    let mut sizes = vec![441, 480, 32, 2048, 1000, 479, 481];
    while host.t < 200_000 {
        let n = sizes.pop().unwrap_or_else(|| lcg.size());
        host.run_with(n, noise);
        thread::sleep(Duration::from_micros(200));
    }
    let d = 480usize;
    assert!(
        host.latencies.iter().all(|&l| l == d as f32),
        "latency port constant at 480"
    );
    assert!(host.output[..d].iter().all(|&s| s == 0.0));
    host.assert_exact(d..host.t, "raw", |t| noise(t - d as i64));
    let s = host.stats();
    assert!(
        s.attempts >= 1,
        "the helper tried to connect while audio flowed"
    );
    assert!(!s.connected && s.connects == 0);
}

#[test]
fn processed_output_for_quanta_including_non_multiples_of_480() {
    for (q, latency, depth) in [
        (480, 0, 1),
        (256, 0, 1),
        (441, 0, 1),
        (32, 0, 1),
        (1024, 0, 1),
        (480, 96, 2),
        (2048, 37, 1),
    ] {
        let mode = Mode::Gain {
            gain: 0.5,
            latency,
            depth,
        };
        let (mut host, worker, _p) = connected(q, mode);
        let lp = transport_delay(q as u32, depth);
        let total = (lp + latency) as i64;
        assert_eq!(host.stats().transport_delay, lp, "q={q}");
        let from = host.t;
        host.pump(q, 40, Some(&worker));
        host.assert_exact(from..host.t, &format!("processed q={q}"), |t| {
            0.5 * sine(t - total)
        });
        assert_eq!(
            *host.latencies.last().unwrap(),
            total as f32,
            "latency port q={q}"
        );
        let (at, step) = host.max_step(0);
        assert!(
            step < MAX_STEP,
            "q={q}: step {step} at {at} (connect crossfade)"
        );
        assert_eq!(host.stats().misses, 0, "q={q}");
        // The plugin publishes the transport delay it plays out with.
        assert_eq!(
            worker.st.transport.load(Ordering::SeqCst),
            u64::from(lp),
            "published transport delay q={q}"
        );
    }
}

#[test]
fn a_missed_block_is_crossfaded_and_counted() {
    let (mut host, worker, _p) = connected(480, GAIN);
    host.pump(480, 10, Some(&worker));
    // The block after the next one the worker will see.
    let missing = worker.st.last_seq.load(Ordering::SeqCst) + 3;
    worker.skip(missing);
    let from = host.t;
    host.pump(480, 20, Some(&worker));
    let s = host.stats();
    assert_eq!(s.misses, 1);
    let (at, step) = host.max_step(from - 480);
    assert!(step < MAX_STEP, "step {step} at {at}");
    // Somewhere the output was not the processed signal (raw was mixed in)...
    let deviated = (from..host.t)
        .filter(|&t| host.output[t] != 0.5 * sine(t as i64 - 480))
        .count();
    assert!(
        deviated >= 480,
        "the missing block was played (mostly) raw: {deviated}"
    );
    // ...and processing is fully back for the last blocks.
    host.assert_exact(host.t - 4 * 480..host.t, "processed after the miss", |t| {
        0.5 * sine(t - 480)
    });
    assert!(host.latencies.iter().rev().take(25).all(|&l| l == 480.0));
}

#[test]
fn worker_loss_crossfades_to_aligned_raw_and_reconnects() {
    let mode = Mode::Gain {
        gain: 0.5,
        latency: 96,
        depth: 1,
    };
    let (mut host, worker, path) = connected(480, mode);
    host.pump(480, 10, Some(&worker));
    let total = 480 + 96;
    worker.kill();
    let lost_at = host.t;
    host.run_until(480, None, "disconnected", |h| !h.stats().connected);
    host.pump(480, 10, None);
    // Raw, delay-matched to the processed path, at an unchanged latency.
    host.assert_exact(host.t - 5 * 480..host.t, "raw after loss", |t| {
        sine(t - total)
    });
    assert!(
        host.latencies[host.latencies.len() - 15..]
            .iter()
            .all(|&l| l == total as f32)
    );
    // The worker comes back (socket activation would do this).
    let worker = FakeWorker::start(&path, mode, &host);
    host.run_until(480, Some(&worker), "processing again", |h| {
        h.stats().processing
    });
    host.pump(480, 10, Some(&worker));
    host.assert_exact(host.t - 5 * 480..host.t, "processed after reconnect", |t| {
        0.5 * sine(t - total)
    });
    assert_eq!(host.stats().connects, 2);
    let (at, step) = host.max_step(lost_at - 480);
    assert!(step < MAX_STEP, "step {step} at {at}");
}

#[test]
fn an_idle_plugin_does_not_reconnect() {
    let (mut host, worker, path) = connected(480, GAIN);
    host.pump(480, 5, Some(&worker));
    // Audio stops (no more run() calls), then the worker exits.
    worker.kill();
    let deadline = Instant::now() + Duration::from_secs(5);
    while host.stats().connected {
        assert!(Instant::now() < deadline, "helper never saw the hang-up");
        thread::sleep(Duration::from_millis(5));
    }
    thread::sleep(Duration::from_millis(150));
    let attempts = host.stats().attempts;
    let worker = FakeWorker::start(&path, GAIN, &host);
    thread::sleep(Duration::from_millis(400));
    assert_eq!(
        worker.hellos(),
        0,
        "an idle plugin must not activate the worker"
    );
    assert_eq!(host.stats().attempts, attempts);
    // Audio flows again: it reconnects (the first runs clear the stale
    // "processing" state of the last run before the pause).
    host.pump(480, 3, None);
    host.run_until(480, Some(&worker), "processing after audio resumed", |h| {
        h.stats().processing && h.stats().connects == 2
    });
    assert_eq!(worker.hellos(), 1);
}

#[test]
fn a_refused_version_stays_passthrough() {
    let path = socket_path();
    let mut host = Host::new(ShimConfig {
        refused_retry: Duration::from_secs(60),
        ..test_config(&path)
    });
    let worker = FakeWorker::start(&path, Mode::OtherVersion, &host);
    let deadline = Instant::now() + Duration::from_millis(600);
    while Instant::now() < deadline {
        host.run(480);
        thread::sleep(Duration::from_millis(1));
    }
    let s = host.stats();
    assert_eq!(worker.hellos(), 1, "no retry within the refusal back-off");
    assert_eq!(s.refusals, 1);
    assert!(!s.connected && !s.processing);
    host.assert_exact(480..host.t, "raw", |t| sine(t - 480));
    assert!(host.latencies.iter().all(|&l| l == 480.0));
}

#[test]
fn bypass_crossfades_to_raw_and_stops_sending() {
    let (mut host, worker, _p) = connected(480, GAIN);
    host.pump(480, 5, Some(&worker));
    *host.bypass = 1.0;
    let from = host.t;
    host.pump(480, 10, Some(&worker));
    let sent = host.stats().blocks_sent;
    host.pump(480, 10, Some(&worker));
    assert_eq!(host.stats().blocks_sent, sent, "bypass sends nothing");
    host.assert_exact(host.t - 10 * 480..host.t, "raw while bypassed", |t| {
        sine(t - 480)
    });
    let (at, step) = host.max_step(from);
    assert!(step < MAX_STEP, "step {step} at {at}");
    *host.bypass = 0.0;
    host.run_until(480, Some(&worker), "processing after bypass", |h| {
        h.stats().processing
    });
}

#[test]
fn a_stalled_worker_is_dropped_and_retried() {
    let path = socket_path();
    let mut host = Host::new(test_config(&path));
    let worker = FakeWorker::start(&path, Mode::Stall, &host);
    host.run_until(480, None, "a second hello after the stall", |_| {
        worker.hellos() >= 2
    });
    // Two hellos prove two connections, but the plugin's counter may lag or
    // lead the worker's read by a moment, and a further retry may already be
    // under way: wait (bounded) for the counter to reach 2, never for equality.
    host.run_until(480, None, "two connects counted", |h| {
        h.stats().connects >= 2
    });
    host.assert_exact(480..host.t, "raw throughout", |t| sine(t - 480));
}

#[test]
fn larger_runs_than_the_maximum_quantum_stay_raw_and_exact() {
    let path = socket_path();
    let mut host = Host::new(test_config(&path));
    let _worker = FakeWorker::start(&path, GAIN, &host);
    for _ in 0..50 {
        host.run(MAX_RUN);
        thread::sleep(Duration::from_millis(1));
    }
    // Whatever the plugin decided, the output never steps.
    let (at, step) = host.max_step(0);
    assert!(step < MAX_STEP, "step {step} at {at}");
}

/// Runs `q`-sample quanta for `d` of wall time, without a worker sync.
fn run_for(host: &mut Host, q: usize, d: Duration) {
    let end = Instant::now() + d;
    while Instant::now() < end {
        host.run(q);
        thread::sleep(Duration::from_micros(500));
    }
}

#[test]
fn quantum_changes_keep_the_largest_transport_delay() {
    // The delay formula is not monotonic in the quantum: 480 needs 480
    // samples, 256 needs 960. Every change must end with the largest delay
    // seen, processed audio exact at it, and no miss.
    let (mut host, worker, _p) = connected(480, GAIN);
    host.pump(480, 10, Some(&worker));
    assert_eq!(host.stats().transport_delay, 480);
    let mut largest = 480;
    for q in [256usize, 480, 1024, 1919, 1024, 256, 480] {
        largest = largest.max(transport_delay(q as u32, 1));
        host.run_until(q, Some(&worker), &format!("processing at q={q}"), |h| {
            h.stats().processing && h.stats().transport_delay == largest
        });
        let from = host.t;
        for _ in 0..30 {
            host.run(q);
            worker.sync(&host);
            assert!(host.stats().processing, "q={q}: processing flipped off");
        }
        assert_eq!(host.stats().transport_delay, largest, "q={q}");
        host.assert_exact(from..host.t, &format!("processed q={q}"), |t| {
            0.5 * sine(t - i64::from(largest))
        });
        assert_eq!(*host.latencies.last().unwrap(), largest as f32, "q={q}");
    }
    assert_eq!(largest, transport_delay(1919, 1));
    assert_eq!(host.stats().misses, 0);
    let (at, step) = host.max_step(0);
    assert!(step < MAX_STEP, "step {step} at {at}");
}

#[test]
fn the_first_fade_in_is_between_aligned_signals() {
    // Depth 2 and a 96-sample algorithmic delay: processed audio is 1056
    // samples late, the raw passthrough before it only 480. The raw path
    // must reach 1056 before processed audio fades in against it.
    let mode = Mode::Gain {
        gain: 0.5,
        latency: 96,
        depth: 2,
    };
    let (mut host, worker, _p) = connected(480, mode);
    host.pump(480, 10, Some(&worker));
    let total = 960 + 96i64;
    // `on`: the first sample from which the output is processed for good.
    let mut on = host.t;
    while on > 0 && host.output[on - 1].to_bits() == (0.5 * sine(on as i64 - 1 - total)).to_bits() {
        on -= 1;
    }
    let fade = 240usize;
    assert!(on > fade + 400, "processing started at {on}");
    // The fade itself mixes raw and processed of the same input sample.
    for t in on - fade + 1..on {
        let g = (t - (on - fade)) as f32 / fade as f32;
        let x = sine(t as i64 - total);
        let expected = g * 0.5 * x + (1.0 - g) * x;
        assert!(
            (host.output[t] - expected).abs() < 1e-6,
            "sample {t} in the fade: {} vs {expected}",
            host.output[t]
        );
    }
    // Before it, the raw path moved from 480 to the processed delay by a
    // crossfade between its two taps (its last step, exactly on the new
    // tap, is the fade-in's first sample, checked above)...
    let r0 = on - 2 * fade + 1;
    for t in r0 + 1..r0 + fade {
        let h = (t - r0) as f32 / fade as f32;
        let expected = h * sine(t as i64 - total) + (1.0 - h) * sine(t as i64 - 480);
        assert!(
            (host.output[t] - expected).abs() < 1e-6,
            "sample {t} in the raw move: {} vs {expected}",
            host.output[t]
        );
    }
    // ...and before that it was the plain 480-sample passthrough.
    host.assert_exact(480..r0 + 1, "raw before the move", |t| sine(t - 480));
    let (at, step) = host.max_step(0);
    assert!(step < MAX_STEP, "step {step} at {at}");
}

#[test]
fn bypass_disconnects_the_worker_and_does_not_reconnect() {
    let (mut host, worker, _p) = connected(480, GAIN);
    host.pump(480, 5, Some(&worker));
    *host.bypass = 1.0;
    host.run_until(480, None, "disconnected while bypassed", |h| {
        !h.stats().connected
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while worker.st.hangups.load(Ordering::SeqCst) == 0 {
        assert!(
            Instant::now() < deadline,
            "the worker never saw the hang-up"
        );
        thread::sleep(Duration::from_millis(2));
    }
    let attempts = host.stats().attempts;
    // Audio keeps flowing, bypassed: no new connection, no attempt.
    run_for(&mut host, 480, Duration::from_millis(400));
    assert_eq!(worker.hellos(), 1, "bypass must not re-activate the worker");
    assert_eq!(host.stats().attempts, attempts);
    assert!(!host.stats().connected);
    host.assert_exact(host.t - 10 * 480..host.t, "raw while bypassed", |t| {
        sine(t - 480)
    });
    // Leaving bypass connects again.
    *host.bypass = 0.0;
    host.run_until(480, Some(&worker), "processing after bypass", |h| {
        h.stats().processing
    });
    assert_eq!(worker.hellos(), 2);
    let (at, step) = host.max_step(0);
    assert!(step < MAX_STEP, "step {step} at {at}");
}

#[test]
fn cleanup_during_a_hung_handshake_returns_promptly() {
    // A worker socket that never accepts: the connect lands in the backlog
    // and the hello is never answered, with a 30 s handshake timeout.
    let path = socket_path();
    let listener = Listener::bind(&path).unwrap();
    let mut host = Host::new(ShimConfig {
        connect_timeout: Duration::from_secs(30),
        ..test_config(&path)
    });
    host.run_until(480, None, "a connection attempt", |h| {
        h.stats().attempts >= 1
    });
    thread::sleep(Duration::from_millis(50));
    let start = Instant::now();
    drop(host);
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "cleanup took {:?}",
        start.elapsed()
    );

    // The same with the backlog full: the connect fails at once.
    let mut queued = Vec::new();
    while let Ok((s, _)) = obc_audio::sys::SeqPacket::connect_nonblocking(&path) {
        queued.push(s);
        assert!(queued.len() < 1024, "the backlog never filled");
    }
    let mut host = Host::new(ShimConfig {
        connect_timeout: Duration::from_secs(30),
        ..test_config(&path)
    });
    host.run_until(480, None, "connection attempts", |h| {
        h.stats().attempts >= 2
    });
    let start = Instant::now();
    drop(host);
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "cleanup took {:?}",
        start.elapsed()
    );
    drop(listener);
}

/// Runs a host against a misbehaving worker; the output must stay
/// continuous, nothing may panic, and the plugin must survive it.
fn misbehaving(bad: Bad, good: u64) -> (Host, FakeWorker) {
    let path = socket_path();
    let mut host = Host::new(test_config(&path));
    let worker = FakeWorker::start(&path, Mode::Bad { bad, good }, &host);
    run_for(&mut host, 480, Duration::from_millis(600));
    let (at, step) = host.max_step(0);
    assert!(step < MAX_STEP, "{bad:?}: step {step} at {at}");
    (host, worker)
}

#[test]
fn corrupt_counters_drop_the_link_play_raw_and_reconnect() {
    for bad in [Bad::OutHeadAhead, Bad::InTailAhead] {
        // Misbehaving from the first block: raw throughout, reconnecting.
        let (mut host, worker) = misbehaving(bad, 0);
        host.run_until(480, None, "reconnects after corruption", |_| {
            worker.hellos() >= 3
        });
        assert!(
            worker.st.hangups.load(Ordering::SeqCst) >= 2,
            "{bad:?}: the plugin hung up"
        );
        assert!(!host.stats().processing);
        host.assert_exact(480..host.t, &format!("{bad:?}: raw"), |t| sine(t - 480));
        // Misbehaving after processing was on: crossfades to raw, recovers.
        let (mut host, worker) = misbehaving(bad, 40);
        host.run_until(480, None, "reconnects after corruption", |_| {
            worker.hellos() >= 2
        });
        let (at, step) = host.max_step(0);
        assert!(step < MAX_STEP, "{bad:?}: step {step} at {at}");
    }
}

#[test]
fn wrong_sequence_numbers_and_sizes_are_ignored_and_play_raw() {
    for bad in [Bad::WrongSeq, Bad::WrongN] {
        let (host, worker) = misbehaving(bad, 0);
        let s = host.stats();
        assert!(s.connected, "{bad:?}: the link stays (the worker is alive)");
        assert!(!s.processing && s.misses > 0, "{bad:?}: {s:?}");
        assert_eq!(worker.hellos(), 1);
        host.assert_exact(480..host.t, &format!("{bad:?}: raw"), |t| sine(t - 480));
    }
}

#[test]
fn an_absurd_latency_is_clamped() {
    let (host, _worker) = misbehaving(Bad::HugeLatency, 0);
    let max = (transport_delay(480, 1) + obc_audio::MAX_LATENCY_SAMPLES) as f32;
    assert!(
        host.latencies.iter().all(|&l| l <= max),
        "latency port within range"
    );
    assert_eq!(*host.latencies.last().unwrap(), max);
}
