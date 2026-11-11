//! `libomarchy_broadcast_mic.so`: the LADSPA forwarding plugin of the
//! Omarchy Broadcast Mic host.
//!
//! The mic host's filter-chain runs this plugin on its real-time thread.
//! It forwards each 480-sample block of the microphone to the audio worker
//! over the shared-memory transport of [`obc_audio`], and plays back the
//! worker's processed blocks after a constant transport delay. Whenever a
//! processed block is not there in time (no worker, the worker crashed,
//! stalled or is still loading, a refusal, bypass) it plays the raw input,
//! delay-matched, with a 5 ms crossfade at every switch: the output is
//! never silence and never steps. The plugin loads no vendor code.
//!
//! # Ports
//!
//! | index | name | kind | meaning |
//! |---|---|---|---|
//! | 0 | `In` | audio in | mono microphone |
//! | 1 | `Out` | audio out | processed or raw |
//! | 2 | `latency` | control out | delay the plugin adds, in samples |
//! | 3 | `bypass` | control in | above 0.5: raw only, no worker connection |
//!
//! The delay starts at 480 samples (10 ms) and changes only while the
//! stream it affects is inaudible: it becomes `transport delay + the
//! worker's algorithmic delay` once processed audio is fully on (see
//! [`engine`]). Only 48 kHz is processed; at any other rate the plugin is a
//! fixed 480-sample delay.
//!
//! The port is written at `connect_port` and `activate` as well as after
//! every `run()`. Observed with PipeWire 1.6.9: filter-graph propagates the
//! value it finds when the graph starts (480) to the nodes'
//! `ProcessLatency`, but not later changes made by `run()`; the delay while
//! processing must be reported some other way.
//!
//! # Threads
//!
//! `run()` never allocates, locks or blocks: its only syscalls are one
//! non-blocking send on the wake socket to the worker per callback that
//! sent blocks, and an eventfd write to the plugin's own helper thread when
//! it needs a connection. Every buffer it touches is prefaulted at
//! `instantiate` (the shared rings when the helper maps them). If the
//! helper's state cannot be created (no eventfd), the instance is still
//! created and is a plain 480-sample delay. The helper
//! (spawned at `instantiate`, joined at `cleanup`) connects, reconnects
//! and retires connections ([`helper`]).

pub mod engine;
pub mod helper;
pub mod ladspa;

use std::ffi::c_ulong;
use std::ptr;
use std::sync::Arc;
use std::thread::JoinHandle;

use obc_audio::{MAX_QUANTUM, SAMPLE_RATE};

pub use helper::{ShimConfig, ShimStats};
use ladspa::{Descriptor, LadspaData, LadspaHandle, port};

/// One plugin instance (the `LADSPA_Handle`).
struct Instance {
    ports: [*mut LadspaData; port::COUNT],
    engine: engine::Engine,
    /// `None` if the helper's state could not be created: passthrough only.
    shared: Option<Arc<helper::Shared>>,
    helper: Option<JoinHandle<()>>,
    input: Box<[f32]>,
    output: Box<[f32]>,
    /// The host's rate is 48 kHz: the worker may be used.
    rate_ok: bool,
}

impl Instance {
    /// Writes the current delay to the `latency` port, if connected.
    fn write_latency(&mut self) {
        // SAFETY: a connected control port points at one float that the
        // host keeps valid while the instance lives.
        if let Some(lat) = unsafe { self.ports[port::LATENCY].as_mut() } {
            *lat = self.engine.latency_port() as LadspaData;
        }
    }
}

/// The LADSPA entry point: descriptor 0 is the plugin, any other index null.
#[unsafe(no_mangle)]
pub extern "C" fn ladspa_descriptor(index: c_ulong) -> *const Descriptor {
    if index == 0 {
        &ladspa::DESCRIPTOR.0
    } else {
        ptr::null()
    }
}

pub(crate) unsafe extern "C" fn instantiate(
    _desc: *const Descriptor,
    rate: c_ulong,
) -> LadspaHandle {
    let shared = helper::Shared::new().ok().map(Arc::new);
    Box::into_raw(new_instance(rate, shared)).cast()
}

/// Builds an instance; `shared: None` gives a passthrough-only instance
/// (no helper, never a worker), which `instantiate` falls back to rather
/// than failing.
fn new_instance(rate: c_ulong, shared: Option<Arc<helper::Shared>>) -> Box<Instance> {
    let rate_ok = rate == c_ulong::from(SAMPLE_RATE);
    let helper = match &shared {
        Some(shared) if rate_ok => {
            let s = Arc::clone(shared);
            std::thread::Builder::new()
                .name("obc-mic-link".into())
                .spawn(move || helper::helper_main(s))
                .ok()
        }
        _ => None,
    };
    let rate_ok = rate_ok && helper.is_some();
    let mut instance = Box::new(Instance {
        ports: [ptr::null_mut(); port::COUNT],
        engine: engine::Engine::new(),
        shared,
        helper,
        input: vec![0.0; MAX_QUANTUM as usize].into_boxed_slice(),
        output: vec![0.0; MAX_QUANTUM as usize].into_boxed_slice(),
        rate_ok,
    });
    instance.engine.prefault();
    // Written, not just zero-allocated: every page is backed now.
    instance.input.fill(0.5);
    instance.output.fill(0.5);
    instance
}

/// # Safety
/// `handle` is a live handle from `instantiate`, not used concurrently.
unsafe fn instance<'a>(handle: LadspaHandle) -> Option<&'a mut Instance> {
    // SAFETY: the caller's contract; null is tolerated.
    unsafe { handle.cast::<Instance>().as_mut() }
}

pub(crate) unsafe extern "C" fn connect_port(
    handle: LadspaHandle,
    port: c_ulong,
    data: *mut LadspaData,
) {
    // SAFETY: the LADSPA contract: a live handle, not used concurrently.
    let Some(inst) = (unsafe { instance(handle) }) else {
        return;
    };
    if let Some(slot) = inst.ports.get_mut(port as usize) {
        *slot = data;
    }
    // Hosts may read the latency port before the first run().
    inst.write_latency();
}

pub(crate) unsafe extern "C" fn activate(handle: LadspaHandle) {
    // SAFETY: the LADSPA contract: a live handle, not used concurrently.
    let Some(inst) = (unsafe { instance(handle) }) else {
        return;
    };
    inst.engine.reset();
    inst.write_latency();
}

pub(crate) unsafe extern "C" fn deactivate(_handle: LadspaHandle) {}

pub(crate) unsafe extern "C" fn run(handle: LadspaHandle, samples: c_ulong) {
    // SAFETY: the LADSPA contract: a live handle, not used concurrently.
    let Some(inst) = (unsafe { instance(handle) }) else {
        return;
    };
    let (inp, outp) = (inst.ports[port::INPUT], inst.ports[port::OUTPUT]);
    if inp.is_null() || outp.is_null() {
        return;
    }
    // SAFETY: control ports, when connected, point at one float that the
    // host keeps valid while it runs the plugin.
    let bypass = unsafe { inst.ports[port::BYPASS].as_ref() }.is_some_and(|&b| b > 0.5);
    let total = samples as usize;
    let quantum = u32::try_from(samples).unwrap_or(u32::MAX);

    let shared = inst.shared.as_deref();
    let link_ptr = shared.map_or(ptr::null_mut(), helper::Shared::enter);
    // SAFETY: between enter() and leave() the helper does not free the
    // link (see helper's module docs) and never touches it otherwise, so
    // this thread has exclusive access to it.
    let mut link = if inst.rate_ok {
        unsafe { link_ptr.as_mut() }
    } else {
        None
    };
    let mut corrupt = false;
    let mut done = 0;
    while done < total {
        let len = (total - done).min(MAX_QUANTUM as usize);
        // Copy in and out through scratch buffers: the host may pass the
        // same buffer for input and output (in-place).
        // SAFETY: the host guarantees `samples` valid floats behind both
        // audio ports; `ptr::copy` tolerates overlap.
        unsafe { ptr::copy(inp.add(done), inst.input.as_mut_ptr(), len) };
        let outcome = inst.engine.process(
            &inst.input[..len],
            &mut inst.output[..len],
            quantum,
            bypass,
            link.as_deref_mut(),
        );
        // SAFETY: as above.
        unsafe { ptr::copy(inst.output.as_ptr(), outp.add(done), len) };
        if outcome.corrupt {
            corrupt = true;
            link = None;
        }
        done += len;
    }
    let heartbeat = link.as_deref().map(|l| l.rings.heartbeat());
    let have_link = link.is_some() || corrupt;
    if let Some(shared) = inst.shared.as_deref() {
        shared.publish(&inst.engine);
        shared.set_wants_link(!bypass && inst.rate_ok);
        shared.leave(heartbeat, corrupt);
        if !have_link && !bypass && inst.rate_ok {
            shared.wake();
        }
    }
    inst.write_latency();
}

pub(crate) unsafe extern "C" fn cleanup(handle: LadspaHandle) {
    if handle.is_null() {
        return;
    }
    // SAFETY: `handle` came from Box::into_raw in `instantiate`; LADSPA
    // calls cleanup once, last.
    let inst = unsafe { Box::from_raw(handle.cast::<Instance>()) };
    if let Some(shared) = &inst.shared {
        shared.request_shutdown();
    }
    if let Some(h) = inst.helper {
        let _ = h.join();
    }
    if let Some(shared) = &inst.shared {
        shared.retire();
    }
}

/// Replaces an instance's connection settings (the socket path, timings).
/// Takes effect at the next connection attempt. For tests and embedding;
/// LADSPA hosts never call it.
///
/// # Safety
/// `handle` must be a live handle returned by this plugin's `instantiate`,
/// and no other call on it (`run()` included) may be in progress.
pub unsafe fn configure(handle: LadspaHandle, config: ShimConfig) {
    // SAFETY: the caller's contract. Only the `Arc<Shared>` is touched,
    // which is shared with the helper by design.
    let shared = unsafe { &(*handle.cast::<Instance>()).shared };
    if let Some(Ok(mut c)) = shared.as_deref().map(|s| s.config.lock()) {
        *c = config;
    }
}

/// An instance's state snapshot.
///
/// # Safety
/// `handle` must be a live handle returned by this plugin's `instantiate`,
/// and no other call on it (`run()` included) may be in progress.
pub unsafe fn stats(handle: LadspaHandle) -> ShimStats {
    // SAFETY: the caller's contract; only the shared atomics are read.
    let shared = unsafe { &(*handle.cast::<Instance>()).shared };
    shared
        .as_deref()
        .map_or_else(ShimStats::default, helper::Shared::stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_shared_state_the_instance_is_a_plain_delay() {
        let h: LadspaHandle = Box::into_raw(new_instance(48_000, None)).cast();
        assert!(!h.is_null());
        let mut input: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.37).sin()).collect();
        let mut output = vec![9.0f32; 4800];
        let mut latency = -1.0f32;
        let mut bypass = 0.0f32;
        // SAFETY: a live handle and buffers that outlive it, as a host would.
        unsafe {
            connect_port(h, port::INPUT as c_ulong, input.as_mut_ptr());
            connect_port(h, port::OUTPUT as c_ulong, output.as_mut_ptr());
            connect_port(h, port::LATENCY as c_ulong, &mut latency);
            connect_port(h, port::BYPASS as c_ulong, &mut bypass);
            activate(h);
            for chunk in 0..10 {
                connect_port(
                    h,
                    port::INPUT as c_ulong,
                    input.as_mut_ptr().add(chunk * 480),
                );
                connect_port(
                    h,
                    port::OUTPUT as c_ulong,
                    output.as_mut_ptr().add(chunk * 480),
                );
                run(h, 480);
            }
            assert_eq!(stats(h), ShimStats::default());
            configure(h, ShimConfig::default());
            deactivate(h);
            cleanup(h);
        }
        assert_eq!(latency, 480.0);
        assert!(output[..480].iter().all(|&s| s == 0.0));
        for i in 480..4800 {
            assert_eq!(output[i].to_bits(), input[i - 480].to_bits(), "sample {i}");
        }
    }
}
