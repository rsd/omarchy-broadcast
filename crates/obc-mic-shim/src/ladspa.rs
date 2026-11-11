//! The LADSPA 1.1 C types this plugin needs, declared from the
//! specification's ABI (field order and constant values of `ladspa.h`), and
//! the plugin's static descriptor.

use std::ffi::{c_char, c_int, c_ulong, c_void};

/// `LADSPA_Data`.
pub type LadspaData = f32;
/// `LADSPA_Handle`.
pub type LadspaHandle = *mut c_void;

/// `LADSPA_PROPERTY_REALTIME`: the plugin has a real-time dependency (in
/// the specification's words, "e.g. listens to a MIDI device"), so its
/// output must not be cached or subject to significant latency. It does
/// *not* mean "suitable for real-time use"; that is
/// [`PROPERTY_HARD_RT_CAPABLE`].
pub const PROPERTY_REALTIME: c_int = 0x1;
/// `LADSPA_PROPERTY_HARD_RT_CAPABLE`: `run()` does not allocate, lock,
/// wait or do I/O that might block, and its time depends only on the
/// sample count.
pub const PROPERTY_HARD_RT_CAPABLE: c_int = 0x4;

/// `LADSPA_PORT_INPUT`.
pub const PORT_INPUT: c_int = 0x1;
/// `LADSPA_PORT_OUTPUT`.
pub const PORT_OUTPUT: c_int = 0x2;
/// `LADSPA_PORT_CONTROL`.
pub const PORT_CONTROL: c_int = 0x4;
/// `LADSPA_PORT_AUDIO`.
pub const PORT_AUDIO: c_int = 0x8;

/// `LADSPA_HINT_BOUNDED_BELOW`.
pub const HINT_BOUNDED_BELOW: c_int = 0x1;
/// `LADSPA_HINT_BOUNDED_ABOVE`.
pub const HINT_BOUNDED_ABOVE: c_int = 0x2;
/// `LADSPA_HINT_TOGGLED`.
pub const HINT_TOGGLED: c_int = 0x4;
/// `LADSPA_HINT_INTEGER`.
pub const HINT_INTEGER: c_int = 0x20;
/// `LADSPA_HINT_DEFAULT_0`.
pub const HINT_DEFAULT_0: c_int = 0x200;

/// `LADSPA_PortRangeHint`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PortRangeHint {
    /// `HintDescriptor`.
    pub hint_descriptor: c_int,
    /// `LowerBound`.
    pub lower_bound: LadspaData,
    /// `UpperBound`.
    pub upper_bound: LadspaData,
}

/// `LADSPA_Descriptor`.
#[repr(C)]
#[derive(Debug)]
pub struct Descriptor {
    /// `UniqueID`.
    pub unique_id: c_ulong,
    /// `Label`.
    pub label: *const c_char,
    /// `Properties`.
    pub properties: c_int,
    /// `Name`.
    pub name: *const c_char,
    /// `Maker`.
    pub maker: *const c_char,
    /// `Copyright`.
    pub copyright: *const c_char,
    /// `PortCount`.
    pub port_count: c_ulong,
    /// `PortDescriptors`.
    pub port_descriptors: *const c_int,
    /// `PortNames`.
    pub port_names: *const *const c_char,
    /// `PortRangeHints`.
    pub port_range_hints: *const PortRangeHint,
    /// `ImplementationData`.
    pub implementation_data: *mut c_void,
    /// `instantiate`.
    pub instantiate: Option<unsafe extern "C" fn(*const Descriptor, c_ulong) -> LadspaHandle>,
    /// `connect_port`.
    pub connect_port: Option<unsafe extern "C" fn(LadspaHandle, c_ulong, *mut LadspaData)>,
    /// `activate`.
    pub activate: Option<unsafe extern "C" fn(LadspaHandle)>,
    /// `run`.
    pub run: Option<unsafe extern "C" fn(LadspaHandle, c_ulong)>,
    /// `run_adding` (not provided).
    pub run_adding: Option<unsafe extern "C" fn(LadspaHandle, c_ulong)>,
    /// `set_run_adding_gain` (not provided).
    pub set_run_adding_gain: Option<unsafe extern "C" fn(LadspaHandle, LadspaData)>,
    /// `deactivate`.
    pub deactivate: Option<unsafe extern "C" fn(LadspaHandle)>,
    /// `cleanup`.
    pub cleanup: Option<unsafe extern "C" fn(LadspaHandle)>,
}

/// The plugin's ports, in index order.
pub mod port {
    /// Mono audio input.
    pub const INPUT: usize = 0;
    /// Mono audio output.
    pub const OUTPUT: usize = 1;
    /// Output control: the delay the plugin adds, in samples.
    pub const LATENCY: usize = 2;
    /// Input control: above 0.5, never process (raw passthrough at the
    /// same delay, no worker connection).
    pub const BYPASS: usize = 3;
    /// The number of ports.
    pub const COUNT: usize = 4;
}

/// The plugin's LADSPA label (the `label` of the filter-chain node).
pub const LABEL: &std::ffi::CStr = c"omarchy_broadcast_mic";
/// The plugin's LADSPA unique ID (not registered with ladspa.org; chosen
/// outside the commonly used ranges).
pub const UNIQUE_ID: c_ulong = 4_242_017;

/// Raw pointers in a static: immutable, so sharing them is sound.
#[repr(transparent)]
pub(crate) struct SyncStatic<T>(pub T);
// SAFETY: the wrapped values are never mutated and only point at other
// immutable statics.
unsafe impl<T> Sync for SyncStatic<T> {}

static PORT_DESCRIPTORS: [c_int; port::COUNT] = [
    PORT_AUDIO | PORT_INPUT,
    PORT_AUDIO | PORT_OUTPUT,
    PORT_CONTROL | PORT_OUTPUT,
    PORT_CONTROL | PORT_INPUT,
];

static PORT_NAMES: SyncStatic<[*const c_char; port::COUNT]> = SyncStatic([
    c"In".as_ptr(),
    c"Out".as_ptr(),
    c"latency".as_ptr(),
    c"bypass".as_ptr(),
]);

static PORT_HINTS: [PortRangeHint; port::COUNT] = [
    PortRangeHint {
        hint_descriptor: 0,
        lower_bound: 0.0,
        upper_bound: 0.0,
    },
    PortRangeHint {
        hint_descriptor: 0,
        lower_bound: 0.0,
        upper_bound: 0.0,
    },
    PortRangeHint {
        hint_descriptor: HINT_BOUNDED_BELOW | HINT_INTEGER,
        lower_bound: 0.0,
        upper_bound: 0.0,
    },
    PortRangeHint {
        hint_descriptor: HINT_TOGGLED | HINT_DEFAULT_0,
        lower_bound: 0.0,
        upper_bound: 1.0,
    },
];

pub(crate) static DESCRIPTOR: SyncStatic<Descriptor> = SyncStatic(Descriptor {
    unique_id: UNIQUE_ID,
    label: LABEL.as_ptr(),
    // REALTIME in its LADSPA sense, deliberately: the output depends on
    // another process answering in wall-clock time (a block not back from
    // the worker within the transport delay plays raw), so a host running
    // the plugin faster than real time, or caching its output, would get a
    // different signal. HARD_RT_CAPABLE: run() never allocates, locks or
    // blocks (its only syscalls are non-blocking sends and eventfd writes).
    properties: PROPERTY_REALTIME | PROPERTY_HARD_RT_CAPABLE,
    name: c"Omarchy Broadcast Mic forwarder".as_ptr(),
    maker: c"omarchy-broadcast".as_ptr(),
    copyright: c"MIT OR Apache-2.0".as_ptr(),
    port_count: port::COUNT as c_ulong,
    port_descriptors: PORT_DESCRIPTORS.as_ptr(),
    port_names: PORT_NAMES.0.as_ptr(),
    port_range_hints: PORT_HINTS.as_ptr(),
    implementation_data: std::ptr::null_mut(),
    instantiate: Some(crate::instantiate),
    connect_port: Some(crate::connect_port),
    activate: Some(crate::activate),
    run: Some(crate::run),
    run_adding: None,
    set_run_adding_gain: None,
    deactivate: Some(crate::deactivate),
    cleanup: Some(crate::cleanup),
});
