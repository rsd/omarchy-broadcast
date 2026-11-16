//! The versioned C ABI between omarchy-broadcast and its modules.
//!
//! A module is a manifest (a TOML file in a `modules.d` directory) plus a
//! shared library that exports two symbols:
//!
//! * [`OBC_MODULE_ABI_VERSION_SYMBOL`], an [`obc_module_abi_version_fn`]:
//!   returns the ABI version the library was built against
//!   ([`OBC_MODULE_ABI_VERSION`]). It has no side effects. The host calls it
//!   first and refuses a library whose major version differs from its own,
//!   or whose minor version is newer, before calling anything else;
//! * [`OBC_MODULE_ENTRY_SYMBOL`], an [`obc_module_entry_fn`]: takes the
//!   host's [`obc_host`] and returns the module's [`obc_module_vtable`], or
//!   null when the module refuses the host.
//!
//! Everything else goes through the vtable. This crate holds only the
//! `#[repr(C)]` types and constants of that boundary, with no logic beyond
//! a few constructors; the C header `include/omarchy_broadcast_module.h` is
//! generated from this file by cbindgen and committed (a test regenerates
//! it and fails when the two differ). Module authors writing Rust use the
//! safe `obc-module-sdk` crate instead of these types.
//!
//! # Stability
//!
//! ABI 1 is stable from 1.6. A module library built for ABI 1.N loads and
//! behaves as documented on every host of ABI 1.M with M >= N; a host
//! refuses a library of a newer minor version, with a reason that says
//! what to update. A minor version only appends (structure fields,
//! vtable entries, constants, manifest keys, JSON keys and types): nothing
//! is removed from major 1 or changes meaning in it, and a break is a new
//! major version. Covered: the layouts, offsets and constant values, the
//! two exported symbols, the required entries, the ownership, lifetime and
//! call rules below, and the manifest, settings, probe and `_host`
//! vocabularies of each minor version. Not covered: which thread makes a
//! call (beyond the threading rules), timing, and anything undocumented.
//!
//! ABI 1 is defined for LP64 little-endian targets (x86_64, aarch64): the
//! sizes and offsets of the header, and the base sizes `OBC_*_SIZE_1_*`,
//! hold there. The header refuses to compile for a target with another
//! pointer size or byte order.
//!
//! # Conventions
//!
//! * Every structure that may grow in a later minor version starts with a
//!   `uint32_t struct_size`, set by whoever fills it in to the size it was
//!   built with. Fields are only ever appended after the current end of a
//!   structure, and padding is spelled out as `reserved` fields that must be
//!   zero, so a reader can tell which fields are present from
//!   `struct_size` alone. A reader accepts any `struct_size` at least the
//!   structure's base size, its size in the minor version that introduced
//!   it ([`OBC_HOST_SIZE_1_0`], [`OBC_FRAME_SIZE_1_0`],
//!   [`OBC_CHAIN_REQUEST_SIZE_1_0`], [`OBC_LOAD_REPORT_SIZE_1_0`],
//!   [`OBC_MODULE_VTABLE_SIZE_1_0`], and for the audio structures of 1.2
//!   [`OBC_AUDIO_BUFFER_SIZE_1_2`], [`OBC_AUDIO_REQUEST_SIZE_1_2`],
//!   [`OBC_AUDIO_LOAD_REPORT_SIZE_1_2`]), larger or smaller than its own,
//!   and reads a field only when it fits in `struct_size`; it rejects a
//!   smaller one ([`OBC_ERR_INVALID`]). A reader compares with the base
//!   size, never with its own `sizeof`, which grows when a field is
//!   appended. A writer writes only the fields that fit in the reader's
//!   `struct_size` when the reader set it (an output structure).
//! * The structures without a `struct_size` ([`obc_plane`], [`obc_cuda`],
//!   [`obc_value`], [`obc_param`], [`obc_timing`], [`obc_status_value`])
//!   sit in arrays or inside other structures: their size and layout are
//!   frozen for the life of major 1, and growth is by a new structure. A
//!   new value kind may use the 8-byte payload of [`obc_value`] as a
//!   pointer and its `reserved0` as a length.
//! * A boolean is a `uint8_t`, 0 for false and 1 for true; a reader takes
//!   any non-zero value as true. (C's `bool` is not used across the
//!   boundary: a byte other than 0 or 1 in a Rust `bool` is undefined
//!   behaviour.)
//! * A failing call returns a non-zero [`obc_status`]; the reason, as text,
//!   is then available from `last_error` (module calls) or
//!   `instance_last_error` (instance calls).
//! * A stage chain fails open: a stage that fails on its own hides nothing,
//!   and the rest of the chain goes on without it ([`OBC_ERR_STAGE`]).
//!   Except the stages that make the matte (the `background` stage and any
//!   stage of the `matte` slot): they hide the room behind the person, so
//!   they never fail open, and the host takes a failure of one as a
//!   failure of the whole chain.
//! * Strings are UTF-8 and NUL-terminated. A string returned by the module
//!   is owned by the module and stays valid until the next call on the same
//!   object (module or instance).
//! * Every function pointer is declared nullable for safety across the
//!   boundary; the documentation of each says whether a module must provide
//!   it. The host refuses a vtable that lacks a required one. The required
//!   set depends on the module's kind (see [`obc_module_vtable`]).
//!
//! # Version gating
//!
//! The host presents itself to a module as a host of the module's declared
//! minor version (what `obc_module_abi_version` returns): it sets no flag
//! bit, passes no constant (frame kind, pixel format, value kind, matrix,
//! chroma siting, audio format), fills no field, sets no `struct_size`
//! larger than that version's, sends no settings type or status value and
//! calls no vtable entry that the declared minor version does not define,
//! whatever the size of the module's vtable says. A module, for its part:
//!
//! * ignores parameters whose key it does not know, and the members of
//!   `_host` it does not know;
//! * accepts a `struct_size` larger than its own;
//! * answers a constant it does not know with [`OBC_ERR_UNSUPPORTED`].
//!
//! A manifest may say the lowest minor version the module needs
//! (`abi_minor = N`, 0 when absent): a host of an older minor lists such a
//! module as needing module ABI 1.N, and a host refuses a library built for
//! a minor version lower than its manifest's `abi_minor`.
//!
//! # Threading
//!
//! The host serialises the calls on one module object, and the calls on one
//! instance: two calls on one instance never run at the same time. They may
//! come from different threads, one after the other: a module assumes no
//! thread affinity (a chain may be loaded on a worker thread and run on
//! the streaming thread, and a second instance may load while the first
//! runs). Calls on the module and on its instances may run concurrently on
//! different threads, as may calls on different instances.
//! [`obc_host::log`] may be called from any thread.
//!
//! A module may run threads of its own. The module stops (joins) the
//! threads of an instance before `instance_destroy` returns, and those of
//! the module object before `destroy` returns: no code of the module runs
//! after that, except what a later call starts. From such a thread a
//! module calls only [`obc_host::log`] of the host's callbacks, unless the
//! documentation of a callback says otherwise.
//!
//! # GPU contexts
//!
//! For [`OBC_FRAME_CUDA`] frames the host makes [`obc_cuda::context`]
//! current on the calling thread during every instance call. A module
//! never creates a CUDA context of its own for those frames, and never
//! touches the frames' context beyond enqueuing work on the chain's
//! stream. A module whose runtime needs the device's primary context (a
//! TensorRT or ONNX Runtime session, for one) may let the runtime hold it
//! while a GPU session of its own lives, on these terms:
//!
//! * it counts its sessions, and releases (resets) the primary context
//!   when the last one ends;
//! * it restores the context that was current on the calling thread
//!   before it returns;
//! * it makes no GPU call in `init`, `probe` or `check` (which must stay
//!   cheap and must not wake a suspended GPU), nor at all when the
//!   configuration says `_host.cuda = false`.
//!
//! An `audio-filter` module runs in a process of its own and may create a
//! GPU context of its own (see below).
//!
//! # The process
//!
//! A module assumes nothing of the process that hosts it (the video
//! daemon, the audio worker, or a host of its own in a later release): it
//! sets no signal handler, changes no environment variable, working
//! directory or locale, does not fork, and does not exit the process.
//!
//! # Audio (since ABI 1.2)
//!
//! A module of the manifest kind `audio-filter` processes blocks of
//! microphone audio instead of video frames. It is loaded by the audio
//! worker, a process of its own, never by the video daemon. Its instances
//! are made and released by the same entries as video instances
//! (`instance_create`, `instance_destroy`, `instance_unload`,
//! `instance_reset_temporal`, `instance_timings`, `instance_last_error`),
//! but loaded by `audio_instance_load` ([`obc_audio_request`]) and run by
//! `audio_instance_process` ([`obc_audio_buffer`]). The rules of the kind:
//!
//! * `audio_instance_process` runs on the worker's processing thread, which
//!   is not a real-time thread, with a deadline of one block (its
//!   `n_samples` at its `sample_rate`).
//! * A module may create a GPU context of its own (a CUDA context, for
//!   one), but only inside `audio_instance_load` and `instance_unload`:
//!   never in `init`, `probe` or `check`, which must stay cheap and must
//!   not wake a suspended GPU. The worker is a dedicated process, so the
//!   video rule above does not bind it.
//! * A stage that fails fails open: the stage is dropped and the audio
//!   passes on without it, never silence ([`OBC_ERR_STAGE`]). Audio has no
//!   matte stages.
//! * The host fails a stage open after a run of consecutive overruns
//!   (blocks that missed their deadline; 50 by default, half a second of
//!   10 ms blocks), and reports it.
//! * The host refuses an `audio-filter` module whose vtable lacks the audio
//!   entries. The video entries `instance_load` and `instance_process` are
//!   not required of the kind: since ABI 1.6 a host accepts them null (an
//!   older host requires them set, so a module that supports hosts of 1.5
//!   or older sets them to stubs that fail with [`OBC_ERR_UNSUPPORTED`]).
//!
//! # The `background` stage
//!
//! The provider of the well-known `background` stage makes the matte and
//! composites the picture itself: the host hands it the camera frame and
//! takes the finished picture back. The host passes it the load-time
//! parameter [`OBC_BACKGROUND_PARAM_MODE`] (a string,
//! [`OBC_BACKGROUND_MODE_BLUR`] or [`OBC_BACKGROUND_MODE_REPLACE`]; a chain
//! without the effect has no `background` stage) and, with every frame
//! while the chain has the stage, the live parameter
//! [`OBC_BACKGROUND_PARAM_BLUR_STRENGTH`] (a float from 0.0 to 1.0, under
//! either mode; a module ignores it under replace). Under replace the
//! background is the request's image ([`obc_chain_request::background`],
//! a host BGR frame of the chain's output size), or a per-frame background
//! (below). Module-side compositing is the contract of the stage for the
//! life of major 1. A later minor version may add, beside it, an optional
//! contract in which the module returns the matte only and the host
//! composites; a module that does not declare it is not affected.
//!
//! # The per-frame background (since ABI 1.4)
//!
//! A chain that replaces the background (its `background` stage under the
//! replace mode) gets its background image at load time
//! ([`obc_chain_request::background`]). A host that shows a video as the
//! background hands the module one background frame at a time through the
//! nullable vtable entry `instance_set_background`:
//!
//! * The host calls it between two `instance_process` calls, only on a
//!   loaded instance whose chain includes `background` under replace.
//! * The frame has the chain's output size and colour description (matrix,
//!   chroma siting, range), is NV12 ([`OBC_FORMAT_NV12`]) and of the
//!   chain's frame kind: CUDA device memory on the chain's context and
//!   stream for a CUDA chain, host memory for a host chain. The module
//!   copies or converts what it keeps.
//! * A host frame is valid only during the call. A CUDA frame is valid in
//!   stream order: the module may enqueue reads of it on the chain's
//!   stream during the call, and the host keeps the memory alive and
//!   unwritten until the work so enqueued has completed. It is never read
//!   after the call on another stream, nor kept.
//! * It replaces the background of the following `instance_process` calls,
//!   until the next call. A null frame returns to the load-time background
//!   image, and so does every load of the chain (a switch as well as a
//!   full reload): after one the load-time image of that load is in force
//!   until the host gives a frame again.
//! * A frame of the wrong size, format or kind is refused with
//!   [`OBC_ERR_INVALID`]. Any failure leaves the previous background in
//!   force.
//!
//! A module of ABI 1.3 or older, or one that leaves the entry null, is
//! never called: the host shows the video background as a still (the
//! load-time image), and reports that it does.
//!
//! # Namespaces
//!
//! * **Stage ids** are one namespace across modules: two modules that
//!   declare one id are two providers of one function. The well-known ids
//!   (the registry) are `background`, `enhance`, `frame-generation`,
//!   `denoise`, `relight`, `hdr`, `eye-contact`, `auto-frame`,
//!   `mic-denoise`, `mic-dereverb`, `mic-enhance`, `mic-focus` and
//!   `mic-bandwidth`. Every id without the private prefix is the
//!   registry's, present or future. A stage of a module's own has the
//!   vendor form `x-<vendor>-<name>` ([`OBC_PRIVATE_STAGE_PREFIX`];
//!   `mic-x-<vendor>-<name>` for an audio stage, whose ids start with
//!   `mic-`), within the 32 bytes of a stage id.
//! * **Settings groups** likewise: the well-known groups are those of the
//!   registry's stages (`cutout`, `video`, `lighting`, `eye_contact`,
//!   `auto_frame`, `onnx` for video; `noise`, `echo`, `focus`,
//!   `bandwidth`, `voice`, `cpu` for audio), and the daemon's own
//!   (`background`, and `input` for audio). A group of a module's own has
//!   the vendor form `x_<vendor>_<name>` ([`OBC_PRIVATE_GROUP_PREFIX`]; a
//!   group id is one setting-id segment, so `_` and not `-`).
//! * A host warns about a module that uses an id of neither form, and
//!   still loads it.
//! * **Configuration keys** of a module's table (`[module."<id>"]`) that
//!   start with `_` ([`OBC_RESERVED_CONFIG_PREFIX`]) are the host's:
//!   `_host` ([`OBC_HOST_CONFIG_KEY`]) carries facts of the host, and a
//!   host passes no other key starting with `_` that it does not define.
//! * **Status value names** that start with `obc.`
//!   ([`OBC_RESERVED_VALUE_PREFIX`]), and any later metadata or event key
//!   that does, are the host's: a host drops such a value a module
//!   reports.

// The names are the C names of the header, as in a `-sys` crate.
#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_void};
use std::marker::{PhantomData, PhantomPinned};

/// The major version of this ABI. A change of the major version is
/// incompatible: the host refuses a module built for another major version.
///
/// ABI 1 is stable from 1.6: a module built for ABI 1.N loads and behaves
/// as documented on every host of ABI 1.M with M >= N. It is defined for
/// LP64 little-endian targets (x86_64, aarch64); this header refuses to
/// compile for another pointer size or byte order. The structures without
/// a `struct_size` (`obc_plane`, `obc_cuda`, `obc_value`, `obc_param`,
/// `obc_timing`, `obc_status_value`) are frozen for the life of major 1.
pub const OBC_MODULE_ABI_MAJOR: u32 = 1;

/// The minor version of this ABI. A minor version only adds (structure
/// fields at the end, vtable entries at the end, constants): the host
/// accepts a module built for its own or an older minor version.
///
/// * 1.1 adds [`OBC_FRAME_FLAG_REPEAT`].
/// * 1.2 adds audio: [`obc_audio_buffer`], [`obc_audio_request`],
///   [`obc_audio_load_report`], their constants and the vtable entries
///   `audio_instance_load` and `audio_instance_process`.
/// * 1.3 adds named status values: [`obc_status_value`] and the vtable
///   entry `instance_status_values`.
/// * 1.4 adds the per-frame background: the vtable entry
///   `instance_set_background` (see the crate documentation).
/// * 1.5 adds the load's notice: [`obc_load_report::notice`], the first
///   field appended to a structure ([`OBC_LOAD_REPORT_SIZE_1_0`]).
/// * 1.6, the release from which ABI 1 is stable, adds no capability: the
///   base sizes (`OBC_*_SIZE_1_*`), the names of the `background` stage's
///   parameters and of the namespaces, the required entries per module
///   kind (an `audio-filter` module may leave the video entries null), and
///   the rules of version gating, threads, GPU contexts and the process.
///
/// **Version gating.** The host presents itself to a module as a host of
/// the module's declared minor version: it sets no flag bit, passes no
/// constant (frame kind, pixel format, value kind, matrix, chroma siting,
/// audio format), fills no field, sets no `struct_size` larger than that
/// version's, sends no settings type or status value and calls no vtable
/// entry that the declared version does not define, whatever the size of
/// the module's vtable says. A module, for its part, ignores parameters
/// whose key it does not know and unknown members of `_host`, accepts a
/// `struct_size` larger than its own, and answers a constant it does not
/// know with [`OBC_ERR_UNSUPPORTED`].
///
/// A manifest may say the lowest minor version the module needs
/// (`abi_minor = N`, 0 when absent): a host of an older minor version lists
/// the module as needing module ABI 1.N, and a host refuses a library built
/// for a minor version lower than its manifest's `abi_minor`.
pub const OBC_MODULE_ABI_MINOR: u32 = 6;

/// The packed ABI version this crate describes: `major << 16 | minor`
/// ([`obc_module_abi_pack`]).
pub const OBC_MODULE_ABI_VERSION: u32 = (OBC_MODULE_ABI_MAJOR << 16) | OBC_MODULE_ABI_MINOR;

/// The name of the exported [`obc_module_abi_version_fn`]. The name and the
/// signature of this symbol are the same in every major version.
pub const OBC_MODULE_ABI_VERSION_SYMBOL: &str = "obc_module_abi_version";

/// The name of the exported [`obc_module_entry_fn`].
pub const OBC_MODULE_ENTRY_SYMBOL: &str = "obc_module_entry";

/// Packs an ABI version as `major << 16 | minor`.
pub const fn obc_module_abi_pack(major: u16, minor: u16) -> u32 {
    ((major as u32) << 16) | minor as u32
}

/// The major version of a packed ABI version.
pub const fn obc_module_abi_major(version: u32) -> u16 {
    (version >> 16) as u16
}

/// The minor version of a packed ABI version.
pub const fn obc_module_abi_minor(version: u32) -> u16 {
    (version & 0xffff) as u16
}

/// The type of the exported `obc_module_abi_version` symbol: returns the
/// packed ABI version the module was built against. Side-effect free.
///
/// Its name and its signature are the same in every major version of the
/// ABI, so that a host can tell a library of another major version before
/// it calls anything else of it.
pub type obc_module_abi_version_fn = unsafe extern "C" fn() -> u32;

/// The type of the exported `obc_module_entry` symbol. `host` stays valid,
/// and its callbacks callable, until the library is unloaded. Returns the
/// module's vtable, valid until the library is unloaded, or null when the
/// module refuses the host (a null `host`, a `struct_size` too small, or a
/// different major version).
pub type obc_module_entry_fn =
    unsafe extern "C" fn(host: *const obc_host) -> *const obc_module_vtable;

// ---------------------------------------------------------------------------
// Status codes
// ---------------------------------------------------------------------------

/// The result of a fallible call: [`OBC_OK`], or one of the `OBC_ERR_*`
/// codes, with the reason in `last_error` / `instance_last_error`. A host
/// treats an unknown non-zero code as [`OBC_ERR_FAILED`].
pub type obc_status = i32;

/// Success.
pub const OBC_OK: obc_status = 0;
/// An argument is invalid: a null pointer, a `struct_size` smaller than
/// expected, malformed JSON, a string that is not UTF-8, an unknown stage.
pub const OBC_ERR_INVALID: obc_status = 1;
/// The request is valid but not supported by this module: a frame kind, a
/// pixel format or a geometry it does not handle.
pub const OBC_ERR_UNSUPPORTED: obc_status = 2;
/// What the request needs is not available here: a missing model or
/// runtime, hardware that is not present.
pub const OBC_ERR_UNAVAILABLE: obc_status = 3;
/// The call failed at run time (out of memory, a runtime error).
pub const OBC_ERR_FAILED: obc_status = 4;
/// The call was made in the wrong state, for example `instance_process`
/// before a successful `instance_load`.
pub const OBC_ERR_STATE: obc_status = 5;
/// The module's code panicked or threw; the module caught it at the
/// boundary. The object is still usable, but its state is unspecified: the
/// host should unload it.
pub const OBC_ERR_PANIC: obc_status = 6;
/// One stage of an instance's chain failed on its own; the rest of the
/// chain is intact. Only `instance_load` and `instance_process` (and, for
/// audio, `audio_instance_load` and `audio_instance_process`) return it,
/// and `instance_last_error` is then `<stage id>: <reason>`: the id of the
/// failed stage, one of the request's (for a load) or of the loaded
/// chain's (for a frame), a colon and a space, and the reason. A host that
/// cannot read such a stage id there treats the status as
/// [`OBC_ERR_FAILED`] with the whole text as the reason, and unloads the
/// instance.
///
/// A stage that fails on its own fails open: it hides nothing, so the host
/// goes on without it. Matte stages never fail open: a host treats
/// `OBC_ERR_STAGE` naming the `background` stage, or a stage the manifest
/// puts in the `matte` slot, as [`OBC_ERR_FAILED`], and unloads the
/// instance, so no frame runs without its matte.
///
/// * From `instance_load`: nothing of the request is in force. A first load
///   or a reload (see `instance_load`) leaves the instance unloaded; a
///   switch keeps the previous chain, less the failed stage when the switch
///   could not leave that stage as it was (a new load-time parameter it
///   failed to take). The host may load again without the stage.
/// * From `instance_process`: the module has dropped the stage from the
///   loaded chain, and the next frame runs the chain without it. The output
///   of this frame is not written.
pub const OBC_ERR_STAGE: obc_status = 7;

// ---------------------------------------------------------------------------
// The host
// ---------------------------------------------------------------------------

/// A log level of [`obc_host::log`].
pub type obc_log_level = u32;

/// An error the user should see in the daemon's log.
pub const OBC_LOG_ERROR: obc_log_level = 1;
/// Something unexpected that does not stop the module.
pub const OBC_LOG_WARN: obc_log_level = 2;
/// A normal event (a model loaded, a runtime chosen).
pub const OBC_LOG_INFO: obc_log_level = 3;
/// Detail for debugging.
pub const OBC_LOG_DEBUG: obc_log_level = 4;
/// Very verbose detail.
pub const OBC_LOG_TRACE: obc_log_level = 5;

/// `sizeof(obc_host)` in ABI 1.0: the smallest `struct_size` a module
/// accepts.
pub const OBC_HOST_SIZE_1_0: u32 = 24;

/// What the host gives a module at [`obc_module_entry_fn`].
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct obc_host {
    /// `sizeof(obc_host)` as the host was built.
    pub struct_size: u32,
    /// The host's packed ABI version ([`OBC_MODULE_ABI_VERSION`]).
    pub abi_version: u32,
    /// Passed back unchanged as the first argument of the callbacks.
    pub user: *mut c_void,
    /// Writes one line to the host's log. Nullable: a module drops its log
    /// lines when null. Callable from any thread; `msg` is UTF-8 without a
    /// trailing newline, valid only during the call.
    pub log:
        Option<unsafe extern "C" fn(user: *mut c_void, level: obc_log_level, msg: *const c_char)>,
}

// ---------------------------------------------------------------------------
// Opaque handles
// ---------------------------------------------------------------------------

/// A module object, created by `init` and released by `destroy`. Opaque to
/// the host.
pub struct obc_module {
    _data: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

/// An instance of a module (one chain of its stages for one stream),
/// created by `instance_create` and released by `instance_destroy`. Opaque
/// to the host.
pub struct obc_instance {
    _data: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

/// Where the pixels of an [`obc_frame`] live.
pub type obc_frame_kind = u32;

/// CUDA device memory: each plane's `ptr_or_fd` is a `CUdeviceptr`, in the
/// context of [`obc_frame::cuda`].
pub const OBC_FRAME_CUDA: obc_frame_kind = 1;
/// Host memory: each plane's `ptr_or_fd` is a pointer, readable and (for an
/// output frame) writable by the module during the call.
pub const OBC_FRAME_HOST: obc_frame_kind = 2;
/// A DMA-BUF: each plane's `ptr_or_fd` is a file descriptor, with
/// [`obc_frame::dmabuf_modifier`]. Reserved: no host produces it yet.
pub const OBC_FRAME_DMABUF: obc_frame_kind = 3;

/// The pixel format of an [`obc_frame`].
pub type obc_format = u32;

/// NV12: plane 0 is Y (8 bits), plane 1 is interleaved U and V (8 bits
/// each), subsampled by 2 in both directions.
pub const OBC_FORMAT_NV12: obc_format = 1;
/// I420: plane 0 is Y, plane 1 is U and plane 2 is V (8 bits each), U and
/// V subsampled by 2 in both directions.
pub const OBC_FORMAT_I420: obc_format = 2;
/// BGRA, 8 bits per channel, one plane.
pub const OBC_FORMAT_BGRA: obc_format = 3;
/// RGBA, 8 bits per channel, one plane.
pub const OBC_FORMAT_RGBA: obc_format = 4;
/// Planar RGB, one 32-bit float per sample: planes 0, 1 and 2 are R, G and
/// B, in 0.0 to 1.0.
pub const OBC_FORMAT_RGB_F32_PLANAR: obc_format = 5;
/// BGR, 8 bits per channel, one plane (no alpha).
pub const OBC_FORMAT_BGR: obc_format = 6;

/// A bit of [`obc_frame::flags`]: this is the first frame of a new source
/// (a camera switch or a restart), so temporal state should not carry over.
pub const OBC_FRAME_FLAG_NEW_SOURCE: u32 = 1 << 0;

/// A bit of [`obc_frame::flags`], since ABI 1.1: this frame repeats the
/// camera frame of the previous call. The output clock emits a frame on
/// every tick of the output rate; when no new camera frame has arrived
/// since the last tick, it emits the last one again, and the frame of this
/// call has the same picture as the previous one (it was processed in
/// that call too). A module may use the flag to tell a repeat from a fresh
/// frame, for instance to make an intermediate picture from two fresh
/// frames; one that ignores it processes the repeat as any other frame.
///
/// The host sets it only for a module built for ABI 1.1 or later: a module
/// of ABI 1.0 never sees it, and its repeats are processed as before.
pub const OBC_FRAME_FLAG_REPEAT: u32 = 1 << 1;

/// The [`obc_frame::flags`] bits of ABI 1.0.
pub const OBC_FRAME_FLAGS_1_0: u32 = OBC_FRAME_FLAG_NEW_SOURCE;

/// The [`obc_frame::flags`] bits of this ABI version.
pub const OBC_FRAME_FLAGS_ALL: u32 = OBC_FRAME_FLAG_NEW_SOURCE | OBC_FRAME_FLAG_REPEAT;

/// The most planes a frame has.
pub const OBC_MAX_PLANES: usize = 4;

/// One plane of an [`obc_frame`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct obc_plane {
    /// A host pointer ([`OBC_FRAME_HOST`]), a `CUdeviceptr`
    /// ([`OBC_FRAME_CUDA`]) or a file descriptor ([`OBC_FRAME_DMABUF`]).
    pub ptr_or_fd: u64,
    /// Bytes from the start of one row to the start of the next.
    pub pitch: u64,
    /// Bytes from `ptr_or_fd` to the plane's first row. For host and CUDA
    /// frames the plane starts at the address `ptr_or_fd + offset`.
    pub offset: u64,
}

/// The CUDA handles of a frame or a chain: a `CUcontext` and a `CUstream`.
/// Both null for host frames. A null `stream` with a non-null `context` is
/// the context's legacy default stream.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct obc_cuda {
    /// The `CUcontext` the host makes current around instance calls.
    pub context: *mut c_void,
    /// The `CUstream` to enqueue work on.
    pub stream: *mut c_void,
}

/// `sizeof(obc_frame)` in ABI 1.0: the smallest `struct_size` a reader
/// accepts.
pub const OBC_FRAME_SIZE_1_0: u32 = 160;

/// A frame handed to `instance_process`, or the background image of a
/// chain request. The structure only describes the frame: its pixels belong
/// to the host, and the module may access them only during the call.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct obc_frame {
    /// `sizeof(obc_frame)` as the host was built.
    pub struct_size: u32,
    /// An `OBC_FRAME_*` kind.
    pub kind: obc_frame_kind,
    /// An `OBC_FORMAT_*` format.
    pub format: obc_format,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// How many of `planes` are set (at most [`OBC_MAX_PLANES`]).
    pub n_planes: u32,
    /// The planes, in the order of the format.
    pub planes: [obc_plane; OBC_MAX_PLANES],
    /// The CUDA handles of a CUDA frame; null for other kinds.
    pub cuda: obc_cuda,
    /// The DRM format modifier of a DMA-BUF frame; 0 otherwise.
    pub dmabuf_modifier: u64,
    /// The presentation time in nanoseconds; -1 when unknown.
    pub pts_ns: i64,
    /// `OBC_FRAME_FLAG_*` bits; the others are zero. The host sets only
    /// the bits of the module's ABI version (ABI 1.0:
    /// [`OBC_FRAME_FLAG_NEW_SOURCE`]; 1.1 adds [`OBC_FRAME_FLAG_REPEAT`]).
    pub flags: u32,
    /// Zero.
    pub reserved0: u32,
}

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

/// The type held by an [`obc_value`].
pub type obc_value_kind = u32;

/// [`obc_value_data::b`], 0 or 1.
pub const OBC_VALUE_BOOL: obc_value_kind = 1;
/// [`obc_value_data::i`].
pub const OBC_VALUE_INT: obc_value_kind = 2;
/// [`obc_value_data::f`].
pub const OBC_VALUE_FLOAT: obc_value_kind = 3;
/// [`obc_value_data::s`], a NUL-terminated UTF-8 string owned by the
/// caller.
pub const OBC_VALUE_STRING: obc_value_kind = 4;

/// The payload of an [`obc_value`]; the member read is the one its `kind`
/// names.
#[repr(C)]
#[derive(Clone, Copy)]
pub union obc_value_data {
    /// For [`OBC_VALUE_BOOL`]: 0 for false, 1 for true (a reader takes any
    /// non-zero value as true).
    pub b: u8,
    /// For [`OBC_VALUE_INT`].
    pub i: i64,
    /// For [`OBC_VALUE_FLOAT`].
    pub f: f64,
    /// For [`OBC_VALUE_STRING`]: never null.
    pub s: *const c_char,
}

/// A tagged value: the value of a parameter.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct obc_value {
    /// An `OBC_VALUE_*` kind.
    pub kind: obc_value_kind,
    /// Zero.
    pub reserved0: u32,
    /// The value, as `kind` says.
    pub data: obc_value_data,
}

impl obc_value {
    /// A boolean value.
    pub const fn from_bool(b: bool) -> Self {
        obc_value {
            kind: OBC_VALUE_BOOL,
            reserved0: 0,
            data: obc_value_data { b: b as u8 },
        }
    }

    /// An integer value.
    pub const fn from_int(i: i64) -> Self {
        obc_value {
            kind: OBC_VALUE_INT,
            reserved0: 0,
            data: obc_value_data { i },
        }
    }

    /// A floating-point value.
    pub const fn from_float(f: f64) -> Self {
        obc_value {
            kind: OBC_VALUE_FLOAT,
            reserved0: 0,
            data: obc_value_data { f },
        }
    }

    /// A string value; `s` must be a NUL-terminated UTF-8 string that
    /// outlives every use of the value.
    pub const fn from_str_ptr(s: *const c_char) -> Self {
        obc_value {
            kind: OBC_VALUE_STRING,
            reserved0: 0,
            data: obc_value_data { s },
        }
    }
}

/// One parameter of one stage: a load-time parameter in
/// [`obc_chain_request::params`] or a per-frame one passed to
/// `instance_process`. `stage` and `key` are the `stage` and `key` of the
/// setting the module described.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct obc_param {
    /// The stage id, NUL-terminated.
    pub stage: *const c_char,
    /// The parameter key, NUL-terminated.
    pub key: *const c_char,
    /// The value.
    pub value: obc_value,
}

// ---------------------------------------------------------------------------
// Chains
// ---------------------------------------------------------------------------

/// The YUV colour matrix of a chain's frames.
pub type obc_matrix = u32;

/// ITU-R BT.601.
pub const OBC_MATRIX_BT601: obc_matrix = 1;
/// ITU-R BT.709.
pub const OBC_MATRIX_BT709: obc_matrix = 2;
/// ITU-R BT.2020 (non-constant luminance).
pub const OBC_MATRIX_BT2020: obc_matrix = 3;

/// Where the chroma samples of subsampled YUV frames sit.
pub type obc_chroma_siting = u32;

/// MPEG-2 siting: horizontally co-sited with the left luma sample,
/// vertically centred between two rows (the usual NV12 siting).
pub const OBC_CHROMA_SITING_MPEG2: obc_chroma_siting = 1;
/// JPEG / MPEG-1 siting: centred between luma samples in both directions.
pub const OBC_CHROMA_SITING_JPEG: obc_chroma_siting = 2;
/// Co-sited with the top-left luma sample in both directions.
pub const OBC_CHROMA_SITING_COSITED: obc_chroma_siting = 3;

/// `sizeof(obc_chain_request)` in ABI 1.0: the smallest `struct_size` a
/// module accepts.
pub const OBC_CHAIN_REQUEST_SIZE_1_0: u32 = 104;

/// What `instance_load` loads: a chain of the module's stages, in order,
/// for one frame geometry.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct obc_chain_request {
    /// `sizeof(obc_chain_request)` as the host was built.
    pub struct_size: u32,
    /// Zero.
    pub reserved0: u32,
    /// The stage ids of the chain, in the order they run; `n_stages`
    /// NUL-terminated strings.
    pub stages: *const *const c_char,
    /// The number of `stages`.
    pub n_stages: usize,
    /// The `OBC_FRAME_*` kind of the frames `instance_process` will get.
    pub input_kind: obc_frame_kind,
    /// The `OBC_FORMAT_*` format of the input frames.
    pub input_format: obc_format,
    /// The input width in pixels.
    pub input_width: u32,
    /// The input height in pixels.
    pub input_height: u32,
    /// The format of the output frames (of the same kind as the input).
    pub output_format: obc_format,
    /// The output width in pixels; larger than the input for an upscaler.
    pub output_width: u32,
    /// The output height in pixels.
    pub output_height: u32,
    /// The `OBC_MATRIX_*` YUV matrix of the frames.
    pub matrix: obc_matrix,
    /// The `OBC_CHROMA_SITING_*` siting of the frames' chroma.
    pub chroma_siting: obc_chroma_siting,
    /// 1 for full range (0 to 255), 0 for limited range (Y 16 to 235,
    /// chroma 16 to 240); a reader takes any non-zero value as full range.
    pub full_range: u8,
    /// Zero.
    pub reserved1: [u8; 3],
    /// The CUDA handles of a CUDA chain; null for host chains.
    pub cuda: obc_cuda,
    /// The load-time parameters of every stage of the chain; `n_params`
    /// entries. May be null when `n_params` is 0.
    pub params: *const obc_param,
    /// The number of `params`.
    pub n_params: usize,
    /// The background image of a replacement, as a host BGR frame; null
    /// when there is none. Valid only during the call: the module copies
    /// what it keeps.
    pub background: *const obc_frame,
}

/// `sizeof(obc_load_report)` in ABI 1.0 to 1.4, before
/// [`obc_load_report::notice`] was appended: the smallest `struct_size` a
/// reader accepts.
pub const OBC_LOAD_REPORT_SIZE_1_0: u32 = 16;

/// `sizeof(obc_load_report)` in ABI 1.5, which appended
/// [`obc_load_report::notice`]: the `struct_size` a host of 1.5 or later
/// passes a module of 1.5 or later, and the smallest that has room for
/// `notice`.
pub const OBC_LOAD_REPORT_SIZE_1_5: u32 = 24;

/// What `instance_load` reports. The host sets `struct_size` and zeroes the
/// rest; the module fills in the fields that fit in `struct_size`. A host of
/// ABI 1.4 or older passes [`OBC_LOAD_REPORT_SIZE_1_0`], and so does a host
/// of 1.6 or later to a module of ABI 1.4 or older (version gating), so a
/// module never writes `notice` for it; a module of ABI 1.4 or older never
/// writes it, so the host reads the null it set.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct obc_load_report {
    /// `sizeof(obc_load_report)` of the ABI version the host presents to
    /// the module (see [`OBC_MODULE_ABI_MINOR`]): at most its own.
    pub struct_size: u32,
    /// How long the load took, in milliseconds.
    pub load_ms: f32,
    /// How long the warm-up (the first, slow runs) took, in milliseconds;
    /// negative when there was none.
    pub warmup_ms: f32,
    /// 1 when the load reused models of the instance's previous load, else
    /// 0; a reader takes any non-zero value as 1.
    pub reused_model: u8,
    /// Zero.
    pub reserved0: [u8; 3],
    /// Since ABI 1.5, written only when it fits in `struct_size`: what the
    /// user should know of a load that succeeded, as one line of text (an
    /// engine chosen in the settings that could not run, and the one the
    /// chain runs on in its place), or null for nothing. NUL-terminated,
    /// owned by the instance and valid until the next call on it.
    pub notice: *const c_char,
}

/// How long one step of the last processed frame took.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct obc_timing {
    /// A stage id, or a step of the module's own (`convert-in`, `mask`,
    /// `composite`, `convert-out`). NUL-terminated.
    pub step: *const c_char,
    /// Milliseconds.
    pub ms: f32,
    /// Zero.
    pub reserved0: u32,
}

/// A named numeric value a stage reports about the last processed frame
/// (auto-frame's current zoom, for one). Since ABI 1.3.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct obc_status_value {
    /// The stage id the value belongs to. NUL-terminated.
    pub stage: *const c_char,
    /// The name of the value, unique within the stage. NUL-terminated.
    /// Names starting with `obc.` are the host's: a host drops a value of
    /// such a name.
    pub name: *const c_char,
    /// The value.
    pub value: f64,
    /// Zero.
    pub reserved0: u32,
    /// Zero.
    pub reserved1: u32,
}

// ---------------------------------------------------------------------------
// Audio (ABI 1.2)
// ---------------------------------------------------------------------------

/// The sample format of an [`obc_audio_buffer`]. Since ABI 1.2.
pub type obc_audio_format = u32;

/// 32-bit float samples, nominally in -1.0 to 1.0, one plane per channel.
pub const OBC_AUDIO_F32_PLANAR: obc_audio_format = 1;

/// A bit of [`obc_audio_buffer::flags`]: this is the first block of a new
/// source (a device switch or a restart), so temporal state should not
/// carry over.
pub const OBC_AUDIO_FLAG_NEW_SOURCE: u32 = 1 << 0;

/// A bit of [`obc_audio_buffer::flags`]: this block does not follow the
/// previous one without a gap (a block was dropped between them).
pub const OBC_AUDIO_FLAG_DISCONT: u32 = 1 << 1;

/// The [`obc_audio_buffer::flags`] bits of this ABI version.
pub const OBC_AUDIO_FLAGS_ALL: u32 = OBC_AUDIO_FLAG_NEW_SOURCE | OBC_AUDIO_FLAG_DISCONT;

/// The most channels an audio buffer has.
pub const OBC_MAX_AUDIO_CHANNELS: usize = 8;

/// `sizeof(obc_audio_buffer)` in ABI 1.2, which introduced it: the
/// smallest `struct_size` a reader accepts.
pub const OBC_AUDIO_BUFFER_SIZE_1_2: u32 = 104;

/// A block of audio handed to `audio_instance_process`, as input or
/// output. Since ABI 1.2. The structure only describes the block: its
/// samples belong to the host, and the module may access them only during
/// the call. The planes of the input and those of the output never
/// overlap.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct obc_audio_buffer {
    /// `sizeof(obc_audio_buffer)` as the host was built.
    pub struct_size: u32,
    /// An `OBC_AUDIO_*` format.
    pub format: obc_audio_format,
    /// Samples per second.
    pub sample_rate: u32,
    /// How many of `planes` are set (1 to [`OBC_MAX_AUDIO_CHANNELS`]).
    pub channels: u32,
    /// Samples per channel in this block: the `frame_samples` of the load.
    pub n_samples: u32,
    /// Zero.
    pub reserved0: u32,
    /// One host-owned plane of `n_samples` samples per channel, in channel
    /// order; the others null. Read-only in the input, written by the
    /// module in the output.
    pub planes: [*mut f32; OBC_MAX_AUDIO_CHANNELS],
    /// The capture time of the block's first sample in nanoseconds; -1
    /// when unknown.
    pub pts_ns: i64,
    /// `OBC_AUDIO_FLAG_*` bits of the input; the others are zero. Zero in
    /// the output.
    pub flags: u32,
    /// Output only: how likely the block holds speech, 0.0 to 1.0, as the
    /// module's voice activity detection says; -1.0 when unknown. The host
    /// sets -1.0 in the output before the call, and ignores it in the
    /// input.
    pub voice_probability: f32,
}

/// `sizeof(obc_audio_request)` in ABI 1.2, which introduced it: the
/// smallest `struct_size` a module accepts.
pub const OBC_AUDIO_REQUEST_SIZE_1_2: u32 = 56;

/// What `audio_instance_load` loads: a chain of the module's audio stages,
/// in order, for one block format. Since ABI 1.2.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct obc_audio_request {
    /// `sizeof(obc_audio_request)` as the host was built.
    pub struct_size: u32,
    /// Zero.
    pub reserved0: u32,
    /// The stage ids of the chain, in the order they run; `n_stages`
    /// NUL-terminated strings. A module may run two stages of the request
    /// as one model when it has one that does both (one frame of delay
    /// instead of two).
    pub stages: *const *const c_char,
    /// The number of `stages`.
    pub n_stages: usize,
    /// Samples per second, one of the manifest's `rates` of every stage.
    pub sample_rate: u32,
    /// The channels of the input blocks.
    pub channels_in: u32,
    /// The channels of the output blocks.
    pub channels_out: u32,
    /// The samples per channel of every block the host proposes, one of the
    /// manifest's `frame_samples` of every stage. The module may choose
    /// another one of those in [`obc_audio_load_report::frame_samples`].
    pub frame_samples: u32,
    /// The load-time parameters of every stage of the chain; `n_params`
    /// entries. May be null when `n_params` is 0.
    pub params: *const obc_param,
    /// The number of `params`.
    pub n_params: usize,
}

/// `sizeof(obc_audio_load_report)` in ABI 1.2, which introduced it: the
/// smallest `struct_size` a module accepts.
pub const OBC_AUDIO_LOAD_REPORT_SIZE_1_2: u32 = 24;

/// What `audio_instance_load` reports. Since ABI 1.2. The host sets
/// `struct_size`, sets `frame_samples` to the request's, `warmup_ms` to -1
/// and zeroes the rest; the module fills in the fields that fit in
/// `struct_size`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct obc_audio_load_report {
    /// `sizeof(obc_audio_load_report)` as the host was built.
    pub struct_size: u32,
    /// How long the load took, in milliseconds.
    pub load_ms: f32,
    /// How long the warm-up took, in milliseconds; negative when there was
    /// none.
    pub warmup_ms: f32,
    /// The samples per channel of every block the chain takes: the
    /// request's, or another of the manifest's `frame_samples` the module
    /// chose. The host hands blocks of this size only.
    pub frame_samples: u32,
    /// The chain's algorithmic delay, in samples, its lookahead included:
    /// how much later a sound comes out than it went in.
    pub latency_samples: u32,
    /// 1 when the load reused models of the instance's previous load, else
    /// 0; a reader takes any non-zero value as 1.
    pub reused_model: u8,
    /// Zero.
    pub reserved0: [u8; 3],
}

// ---------------------------------------------------------------------------
// The vtable
// ---------------------------------------------------------------------------

/// `sizeof(obc_module_vtable)` in ABI 1.0 (it ends after `check`): the
/// smallest `struct_size` a host accepts.
pub const OBC_MODULE_VTABLE_SIZE_1_0: u32 = 136;

/// The size of the vtable of ABI 1.2, which appends `audio_instance_load`
/// and `audio_instance_process` (1.1 appends no entry).
pub const OBC_MODULE_VTABLE_SIZE_1_2: u32 = 152;

/// The size of the vtable of ABI 1.3, which appends
/// `instance_status_values`.
pub const OBC_MODULE_VTABLE_SIZE_1_3: u32 = 160;

/// The size of the vtable of ABI 1.4, which appends
/// `instance_set_background` (1.5 and 1.6 append no entry).
pub const OBC_MODULE_VTABLE_SIZE_1_4: u32 = 168;

/// The module's entry points, returned by [`obc_module_entry_fn`].
///
/// Every entry is declared nullable; those marked required must be set. A
/// later minor version only appends entries, so a host reads an entry only
/// when it fits in `struct_size` and the module's declared minor version
/// defines it (see [`OBC_MODULE_ABI_MINOR`]).
///
/// **Required entries per kind.** `init`, `probe`, `describe_settings`,
/// `instance_create`, `instance_destroy`, `instance_reset_temporal`,
/// `instance_unload`, `last_error`, `instance_last_error` and `destroy`
/// are required of every module. `instance_load` and `instance_process` are
/// required of a `video-stage` module; `audio_instance_load` and
/// `audio_instance_process` of an `audio-filter` module. A module of one
/// kind may leave the entries of another kind null (since ABI 1.6; a host
/// of 1.5 or older requires `instance_load` and `instance_process` of every
/// module). A kind added later binds its own entries only.
///
/// **Threads.** Calls on one instance are serialised (never concurrent)
/// and may come from different threads: there is no thread affinity. For
/// CUDA frames the frames' context is current on the calling thread during
/// the call. A module may run threads of its own; it stops (joins) them
/// before `instance_destroy` (an instance's) or `destroy` (the module
/// object's) returns, and from them only `obc_host::log` may be called, unless the
/// documentation of a callback says otherwise.
///
/// **GPU contexts.** A module never creates a CUDA context for the host's
/// frames and never touches their context. Its runtime may hold the
/// device's primary context while a GPU session of the module lives, if
/// the module counts its sessions, resets the primary context when the
/// last one ends, restores the context current on the calling thread
/// before it returns, and makes no GPU call in `init`, `probe` or `check`,
/// nor at all under `_host.cuda = false`. An `audio-filter` module, in a
/// process of its own, may create a context in `audio_instance_load`.
///
/// **The process.** A module assumes nothing of the process that hosts
/// it: it sets no signal handler, changes no environment variable, working
/// directory or locale, does not fork and does not exit.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct obc_module_vtable {
    /// `sizeof(obc_module_vtable)` as the module was built.
    pub struct_size: u32,
    /// The module's packed ABI version, as [`obc_module_abi_version_fn`]
    /// returns it.
    pub abi_version: u32,
    /// Required. Creates the module object from `config_json`, the
    /// module's configuration table as a JSON object (`{}` when there is
    /// none). Cheap: no GPU work. On failure `*out` is null and
    /// `last_error(NULL)` gives the reason.
    pub init: Option<
        unsafe extern "C" fn(config_json: *const c_char, out: *mut *mut obc_module) -> obc_status,
    >,
    /// Required. Says which stages can run here, as JSON:
    /// `{"stages":[{"id":"denoise","available":true},{"id":"relight","available":false,"reason":"Not installed"}]}`.
    /// Filesystem checks only: it must not create a CUDA context, load a
    /// model or otherwise wake a suspended GPU. `*out_json` is owned by the
    /// module. A deeper check, made before a stream starts, is `check`.
    pub probe: Option<
        unsafe extern "C" fn(module: *mut obc_module, out_json: *mut *const c_char) -> obc_status,
    >,
    /// Required. Describes the module's settings as JSON
    /// (`{"groups":[...]}`). `*out_json` is owned by the module.
    pub describe_settings: Option<
        unsafe extern "C" fn(module: *mut obc_module, out_json: *mut *const c_char) -> obc_status,
    >,
    /// Required. Creates an instance. All instances are destroyed before
    /// the module.
    pub instance_create: Option<
        unsafe extern "C" fn(module: *mut obc_module, out: *mut *mut obc_instance) -> obc_status,
    >,
    /// Required. Releases an instance (unloading it first if needed).
    pub instance_destroy: Option<unsafe extern "C" fn(instance: *mut obc_instance)>,
    /// Required of a `video-stage` module (null allowed for another kind
    /// since ABI 1.6). Loads a chain of the module's stages. A load on a loaded
    /// instance is a **switch** when the request keeps the frames of the
    /// loaded chain (the same input kind, input and output formats and
    /// sizes, matrix, chroma siting, range and CUDA handles: only stages,
    /// parameters or the background change), and may reuse the models of
    /// the previous load; with other frames it is a **reload**.
    ///
    /// A failed switch leaves the previous chain loaded and running as it
    /// was, whatever the status, except [`OBC_ERR_STAGE`] (which says what
    /// stays in force) and [`OBC_ERR_PANIC`]. A failed first load or
    /// reload, and a load that panicked, leave the instance unloaded: a
    /// host that takes the instance as unloaded calls `instance_unload`,
    /// which the module must accept after any failure.
    pub instance_load: Option<
        unsafe extern "C" fn(
            instance: *mut obc_instance,
            request: *const obc_chain_request,
            out_report: *mut obc_load_report,
        ) -> obc_status,
    >,
    /// Required of a `video-stage` module (null allowed for another kind
    /// since ABI 1.6). Enqueues one frame through the loaded chain. `out` equals
    /// `in` only for in-place chains. The `n_live` per-frame parameters
    /// (`live` may be null when 0) apply from this frame on.
    pub instance_process: Option<
        unsafe extern "C" fn(
            instance: *mut obc_instance,
            input: *const obc_frame,
            output: *const obc_frame,
            live: *const obc_param,
            n_live: usize,
        ) -> obc_status,
    >,
    /// Required. Forgets temporal state (after a seek or a new source).
    /// `*out_dropped_json` is a JSON array of the stage ids that could not
    /// keep running and were dropped from the chain (usually `[]`), owned
    /// by the module.
    pub instance_reset_temporal: Option<
        unsafe extern "C" fn(
            instance: *mut obc_instance,
            out_dropped_json: *mut *const c_char,
        ) -> obc_status,
    >,
    /// Required. Releases what the last load loaded; the instance can be
    /// loaded again.
    pub instance_unload: Option<unsafe extern "C" fn(instance: *mut obc_instance)>,
    /// Nullable. The timings of the last processed frame: `*out` points to
    /// `*n` entries owned by the instance (null and 0 when there are none).
    pub instance_timings: Option<
        unsafe extern "C" fn(
            instance: *mut obc_instance,
            out: *mut *const obc_timing,
            n: *mut usize,
        ) -> obc_status,
    >,
    /// Reserved, nullable (a module leaves it null). No host of ABI 1.x
    /// calls it, unless a later minor version defines it, and then only for
    /// a module that declares that version. (Meant for: the stream is about
    /// to stop for a while, release what can be rebuilt quickly.)
    pub instance_park: Option<unsafe extern "C" fn(instance: *mut obc_instance) -> obc_status>,
    /// Reserved, nullable, as `instance_park` (meant for: the stream
    /// resumes after a park).
    pub instance_unpark: Option<unsafe extern "C" fn(instance: *mut obc_instance) -> obc_status>,
    /// Required. The reason of the last failed call on `module`, valid
    /// until the next call on it. When `module` is null: the reason of the
    /// last failed `init` (or call made with a null object) on the calling
    /// thread, valid until the next such call on that thread. Never null
    /// (empty when there is no error).
    pub last_error: Option<unsafe extern "C" fn(module: *const obc_module) -> *const c_char>,
    /// Required. The reason of the last failed call on `instance`. Never
    /// null; valid until the next call on the instance.
    pub instance_last_error:
        Option<unsafe extern "C" fn(instance: *const obc_instance) -> *const c_char>,
    /// Required. Releases the module object.
    pub destroy: Option<unsafe extern "C" fn(module: *mut obc_module)>,
    /// Nullable (null: `probe` says all there is to know). Checks more
    /// deeply than `probe` that the `n_stages` stages `stages` (stage ids,
    /// NUL-terminated) can run here, before a stream that runs them starts,
    /// so that a missing runtime is told before the camera opens rather
    /// than when the chain loads. It may open the module's own libraries
    /// (its runtime), but must not create a CUDA context, load a model or
    /// otherwise touch the GPU. [`OBC_OK`] when they can run as far as can
    /// be known without the GPU; otherwise a non-zero status
    /// ([`OBC_ERR_UNAVAILABLE`] for a runtime that is not there), with the
    /// reason in `last_error`.
    pub check: Option<
        unsafe extern "C" fn(
            module: *mut obc_module,
            stages: *const *const c_char,
            n_stages: usize,
        ) -> obc_status,
    >,
    /// Since ABI 1.2. Required for an `audio-filter` module, null for the
    /// others. Loads a chain of the module's audio stages. As for
    /// `instance_load`, a load on a loaded instance is a **switch** when
    /// the request keeps the block format of the loaded chain (the same
    /// `sample_rate`, `channels_in`, `channels_out` and `frame_samples`:
    /// only stages or parameters change), and may reuse its models; with
    /// another block format it is a **reload**. A failed switch leaves the
    /// previous chain loaded and running as it was, except after
    /// [`OBC_ERR_STAGE`] (the previous chain, less the failed stage when the
    /// switch could not leave it as it was) and [`OBC_ERR_PANIC`]. A failed
    /// first load or reload, and a load that panicked, leave the instance
    /// unloaded. A module may create its own GPU context here (see the
    /// crate documentation).
    pub audio_instance_load: Option<
        unsafe extern "C" fn(
            instance: *mut obc_instance,
            request: *const obc_audio_request,
            out_report: *mut obc_audio_load_report,
        ) -> obc_status,
    >,
    /// Since ABI 1.2. Required for an `audio-filter` module, null for the
    /// others. Runs one block through the loaded audio chain, from `input`
    /// into the planes of `output` (which never overlap the input's), with
    /// the block format of the load. The module writes every sample of the
    /// output's planes and may set its `voice_probability`. The `n_live`
    /// per-block parameters (`live` may be null when 0) apply from this
    /// block on. After [`OBC_ERR_STAGE`] the module has dropped the stage,
    /// and the output of this block is not written.
    pub audio_instance_process: Option<
        unsafe extern "C" fn(
            instance: *mut obc_instance,
            input: *const obc_audio_buffer,
            output: *mut obc_audio_buffer,
            live: *const obc_param,
            n_live: usize,
        ) -> obc_status,
    >,
    /// Since ABI 1.3. Nullable (null: the module reports no values). The
    /// status values of the last processed frame: `*out` points to `*n`
    /// entries owned by the instance (null and 0 when there are none),
    /// valid until the next call on the instance.
    pub instance_status_values: Option<
        unsafe extern "C" fn(
            instance: *mut obc_instance,
            out: *mut *const obc_status_value,
            n: *mut usize,
        ) -> obc_status,
    >,
    /// Since ABI 1.4. Nullable (null: the module shows a video background
    /// as the load-time still). Replaces the background of a loaded chain
    /// that includes `background` under replace, for the following
    /// `instance_process` calls until the next call; the host calls it
    /// between two `instance_process` calls. `background` is an NV12 frame
    /// of the chain's output size, colour description and frame kind (CUDA
    /// device memory on the chain's context and stream, or host memory).
    /// A host frame is valid only during the call. A CUDA frame is valid in
    /// stream order: the module may enqueue reads of it on the chain's
    /// stream during the call, and the host keeps it alive and unwritten
    /// until the work so enqueued has completed; it must not be read after
    /// the call on any other stream, nor kept. Either way the module copies
    /// or converts what it keeps. A null `background` returns to the
    /// load-time background image. Every load of the chain (a switch or a
    /// full reload) also returns to the load-time image of that load: a
    /// frame given before it is no longer in force, and the host gives one
    /// again when it wants it. [`OBC_ERR_INVALID`] for a frame of the
    /// wrong size, format or kind; any failure leaves the previous
    /// background in force.
    pub instance_set_background: Option<
        unsafe extern "C" fn(
            instance: *mut obc_instance,
            background: *const obc_frame,
        ) -> obc_status,
    >,
}

// ---------------------------------------------------------------------------
// Names (since ABI 1.6)
// ---------------------------------------------------------------------------
//
// cbindgen emits no string constants: the header defines these names in
// the block that `cbindgen.toml` adds after the includes, and the header
// test checks that the two agree.

/// The well-known stage whose provider makes the matte and composites the
/// picture (see the crate documentation).
pub const OBC_STAGE_BACKGROUND: &str = "background";

/// The load-time parameter of the `background` stage that says which
/// effect it composites: a string, [`OBC_BACKGROUND_MODE_BLUR`] or
/// [`OBC_BACKGROUND_MODE_REPLACE`].
pub const OBC_BACKGROUND_PARAM_MODE: &str = "mode";

/// The `mode` that blurs the room behind the person.
pub const OBC_BACKGROUND_MODE_BLUR: &str = "blur";

/// The `mode` that replaces the room behind the person with the
/// background image (or the per-frame background).
pub const OBC_BACKGROUND_MODE_REPLACE: &str = "replace";

/// The live parameter of the `background` stage with the blur strength: a
/// float from 0.0 to 1.0, with every frame while the chain has the stage.
pub const OBC_BACKGROUND_PARAM_BLUR_STRENGTH: &str = "blur_strength";

/// The prefix of a private stage id, of the vendor form
/// `x-<vendor>-<name>` (`mic-x-<vendor>-<name>` for an audio stage).
pub const OBC_PRIVATE_STAGE_PREFIX: &str = "x-";

/// The prefix of a private settings group id, of the vendor form
/// `x_<vendor>_<name>`.
pub const OBC_PRIVATE_GROUP_PREFIX: &str = "x_";

/// The prefix of the keys of a module's configuration table that are the
/// host's.
pub const OBC_RESERVED_CONFIG_PREFIX: &str = "_";

/// The key of a module's configuration with the facts of the host (a JSON
/// object; a module ignores the members it does not know).
pub const OBC_HOST_CONFIG_KEY: &str = "_host";

/// The prefix of the status value names (and of any later metadata or
/// event key) that are the host's.
pub const OBC_RESERVED_VALUE_PREFIX: &str = "obc.";

/// The well-known stage ids (the registry) of this ABI version; every id
/// without [`OBC_PRIVATE_STAGE_PREFIX`] (after the `mic-` of an audio stage)
/// is the registry's, present or future.
pub const OBC_WELL_KNOWN_STAGES: [&str; 13] = [
    "background",
    "enhance",
    "frame-generation",
    "denoise",
    "relight",
    "hdr",
    "eye-contact",
    "auto-frame",
    "mic-denoise",
    "mic-dereverb",
    "mic-enhance",
    "mic-focus",
    "mic-bandwidth",
];

/// The well-known settings group ids of a video module of this ABI
/// version (`background`, the daemon's own, included).
pub const OBC_WELL_KNOWN_VIDEO_GROUPS: [&str; 7] = [
    "background",
    "cutout",
    "video",
    "lighting",
    "eye_contact",
    "auto_frame",
    "onnx",
];

/// The well-known settings group ids of an audio module of this ABI
/// version (`input`, the daemon's own, included).
pub const OBC_WELL_KNOWN_AUDIO_GROUPS: [&str; 7] = [
    "input",
    "noise",
    "echo",
    "focus",
    "bandwidth",
    "voice",
    "cpu",
];

// ---------------------------------------------------------------------------
// Constructors
// ---------------------------------------------------------------------------

impl obc_cuda {
    /// Null handles, for host frames and chains.
    pub const fn null() -> Self {
        obc_cuda {
            context: std::ptr::null_mut(),
            stream: std::ptr::null_mut(),
        }
    }
}

impl Default for obc_cuda {
    fn default() -> Self {
        obc_cuda::null()
    }
}

impl obc_frame {
    /// A frame with `struct_size` set and everything else zero (no
    /// planes, null CUDA handles, unknown time).
    pub const fn empty() -> Self {
        obc_frame {
            struct_size: size_of::<obc_frame>() as u32,
            kind: 0,
            format: 0,
            width: 0,
            height: 0,
            n_planes: 0,
            planes: [obc_plane {
                ptr_or_fd: 0,
                pitch: 0,
                offset: 0,
            }; OBC_MAX_PLANES],
            cuda: obc_cuda::null(),
            dmabuf_modifier: 0,
            pts_ns: -1,
            flags: 0,
            reserved0: 0,
        }
    }
}

impl Default for obc_frame {
    fn default() -> Self {
        obc_frame::empty()
    }
}

impl obc_chain_request {
    /// A request with `struct_size` set and everything else zero or null.
    pub const fn empty() -> Self {
        obc_chain_request {
            struct_size: size_of::<obc_chain_request>() as u32,
            reserved0: 0,
            stages: std::ptr::null(),
            n_stages: 0,
            input_kind: 0,
            input_format: 0,
            input_width: 0,
            input_height: 0,
            output_format: 0,
            output_width: 0,
            output_height: 0,
            matrix: 0,
            chroma_siting: 0,
            full_range: 0,
            reserved1: [0; 3],
            cuda: obc_cuda::null(),
            params: std::ptr::null(),
            n_params: 0,
            background: std::ptr::null(),
        }
    }
}

impl Default for obc_chain_request {
    fn default() -> Self {
        obc_chain_request::empty()
    }
}

impl obc_load_report {
    /// A report as the host passes it: `struct_size` set, no warm-up.
    pub const fn empty() -> Self {
        obc_load_report {
            struct_size: size_of::<obc_load_report>() as u32,
            load_ms: 0.0,
            warmup_ms: -1.0,
            reused_model: 0,
            reserved0: [0; 3],
            notice: std::ptr::null(),
        }
    }
}

impl Default for obc_load_report {
    /// Every field zero (`struct_size` included) and no notice.
    fn default() -> Self {
        obc_load_report {
            struct_size: 0,
            load_ms: 0.0,
            warmup_ms: 0.0,
            reused_model: 0,
            reserved0: [0; 3],
            notice: std::ptr::null(),
        }
    }
}

impl obc_audio_buffer {
    /// A buffer with `struct_size` set and everything else zero (no
    /// planes), unknown time and unknown voice probability.
    pub const fn empty() -> Self {
        obc_audio_buffer {
            struct_size: size_of::<obc_audio_buffer>() as u32,
            format: 0,
            sample_rate: 0,
            channels: 0,
            n_samples: 0,
            reserved0: 0,
            planes: [std::ptr::null_mut(); OBC_MAX_AUDIO_CHANNELS],
            pts_ns: -1,
            flags: 0,
            voice_probability: -1.0,
        }
    }
}

impl Default for obc_audio_buffer {
    fn default() -> Self {
        obc_audio_buffer::empty()
    }
}

impl obc_audio_request {
    /// A request with `struct_size` set and everything else zero or null.
    pub const fn empty() -> Self {
        obc_audio_request {
            struct_size: size_of::<obc_audio_request>() as u32,
            reserved0: 0,
            stages: std::ptr::null(),
            n_stages: 0,
            sample_rate: 0,
            channels_in: 0,
            channels_out: 0,
            frame_samples: 0,
            params: std::ptr::null(),
            n_params: 0,
        }
    }
}

impl Default for obc_audio_request {
    fn default() -> Self {
        obc_audio_request::empty()
    }
}

impl obc_audio_load_report {
    /// A report as the host passes it for a request of `frame_samples`:
    /// `struct_size` and `frame_samples` set, no warm-up.
    pub const fn empty(frame_samples: u32) -> Self {
        obc_audio_load_report {
            struct_size: size_of::<obc_audio_load_report>() as u32,
            load_ms: 0.0,
            warmup_ms: -1.0,
            frame_samples,
            latency_samples: 0,
            reused_model: 0,
            reserved0: [0; 3],
        }
    }
}

impl obc_host {
    /// A host with `struct_size` and `abi_version` set, no user data and
    /// no log callback.
    pub const fn empty() -> Self {
        obc_host {
            struct_size: size_of::<obc_host>() as u32,
            abi_version: OBC_MODULE_ABI_VERSION,
            user: std::ptr::null_mut(),
            log: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    #[test]
    fn version_packing() {
        assert_eq!(OBC_MODULE_ABI_VERSION, 0x0001_0006);
        assert_eq!(obc_module_abi_pack(1, 6), OBC_MODULE_ABI_VERSION);
        assert_eq!(obc_module_abi_pack(2, 7), 0x0002_0007);
        assert_eq!(obc_module_abi_major(0x0002_0007), 2);
        assert_eq!(obc_module_abi_minor(0x0002_0007), 7);
        assert_eq!(
            obc_module_abi_major(OBC_MODULE_ABI_VERSION) as u32,
            OBC_MODULE_ABI_MAJOR
        );
        assert_eq!(
            obc_module_abi_minor(OBC_MODULE_ABI_VERSION) as u32,
            OBC_MODULE_ABI_MINOR
        );
    }

    #[test]
    fn value_constructors() {
        let v = obc_value::from_int(-3);
        assert_eq!(v.kind, OBC_VALUE_INT);
        // SAFETY: the kind says `i` is the member written.
        assert_eq!(unsafe { v.data.i }, -3);
        let v = obc_value::from_bool(true);
        assert_eq!(v.kind, OBC_VALUE_BOOL);
        // SAFETY: as above, `b`.
        assert_eq!(unsafe { v.data.b }, 1);
        // SAFETY: as above.
        assert_eq!(unsafe { obc_value::from_bool(false).data.b }, 0);
        let v = obc_value::from_float(0.5);
        assert_eq!(v.kind, OBC_VALUE_FLOAT);
        // SAFETY: as above, `f`.
        assert_eq!(unsafe { v.data.f }, 0.5);
        let v = obc_value::from_str_ptr(c"warm".as_ptr());
        assert_eq!(v.kind, OBC_VALUE_STRING);
    }

    #[test]
    fn empty_structs_carry_their_size() {
        assert_eq!(
            obc_frame::empty().struct_size as usize,
            size_of::<obc_frame>()
        );
        assert_eq!(
            obc_chain_request::empty().struct_size as usize,
            size_of::<obc_chain_request>()
        );
        assert_eq!(
            obc_load_report::empty().struct_size as usize,
            size_of::<obc_load_report>()
        );
        assert_eq!(
            obc_host::empty().struct_size as usize,
            size_of::<obc_host>()
        );
        assert_eq!(obc_host::empty().abi_version, OBC_MODULE_ABI_VERSION);
        assert_eq!(
            obc_audio_buffer::empty().struct_size as usize,
            size_of::<obc_audio_buffer>()
        );
        assert_eq!(obc_audio_buffer::empty().voice_probability, -1.0);
        assert_eq!(obc_audio_buffer::empty().pts_ns, -1);
        assert_eq!(
            obc_audio_request::empty().struct_size as usize,
            size_of::<obc_audio_request>()
        );
        let report = obc_audio_load_report::empty(480);
        assert_eq!(
            report.struct_size as usize,
            size_of::<obc_audio_load_report>()
        );
        assert_eq!((report.frame_samples, report.warmup_ms), (480, -1.0));
    }

    /// The layout of every structure, pinned: a change here is an ABI
    /// change (a new major version, or fields appended under a new minor
    /// version).
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn layouts_are_pinned() {
        assert_eq!((size_of::<obc_host>(), align_of::<obc_host>()), (24, 8));
        assert_eq!(offset_of!(obc_host, struct_size), 0);
        assert_eq!(offset_of!(obc_host, abi_version), 4);
        assert_eq!(offset_of!(obc_host, user), 8);
        assert_eq!(offset_of!(obc_host, log), 16);

        assert_eq!((size_of::<obc_plane>(), align_of::<obc_plane>()), (24, 8));
        assert_eq!(offset_of!(obc_plane, ptr_or_fd), 0);
        assert_eq!(offset_of!(obc_plane, pitch), 8);
        assert_eq!(offset_of!(obc_plane, offset), 16);

        assert_eq!((size_of::<obc_cuda>(), align_of::<obc_cuda>()), (16, 8));
        assert_eq!(offset_of!(obc_cuda, context), 0);
        assert_eq!(offset_of!(obc_cuda, stream), 8);

        assert_eq!((size_of::<obc_frame>(), align_of::<obc_frame>()), (160, 8));
        assert_eq!(offset_of!(obc_frame, struct_size), 0);
        assert_eq!(offset_of!(obc_frame, kind), 4);
        assert_eq!(offset_of!(obc_frame, format), 8);
        assert_eq!(offset_of!(obc_frame, width), 12);
        assert_eq!(offset_of!(obc_frame, height), 16);
        assert_eq!(offset_of!(obc_frame, n_planes), 20);
        assert_eq!(offset_of!(obc_frame, planes), 24);
        assert_eq!(offset_of!(obc_frame, cuda), 120);
        assert_eq!(offset_of!(obc_frame, dmabuf_modifier), 136);
        assert_eq!(offset_of!(obc_frame, pts_ns), 144);
        assert_eq!(offset_of!(obc_frame, flags), 152);
        assert_eq!(offset_of!(obc_frame, reserved0), 156);

        assert_eq!(
            (size_of::<obc_value_data>(), align_of::<obc_value_data>()),
            (8, 8)
        );
        assert_eq!((size_of::<obc_value>(), align_of::<obc_value>()), (16, 8));
        assert_eq!(offset_of!(obc_value, kind), 0);
        assert_eq!(offset_of!(obc_value, reserved0), 4);
        assert_eq!(offset_of!(obc_value, data), 8);

        assert_eq!((size_of::<obc_param>(), align_of::<obc_param>()), (32, 8));
        assert_eq!(offset_of!(obc_param, stage), 0);
        assert_eq!(offset_of!(obc_param, key), 8);
        assert_eq!(offset_of!(obc_param, value), 16);

        assert_eq!(
            (
                size_of::<obc_chain_request>(),
                align_of::<obc_chain_request>()
            ),
            (104, 8)
        );
        assert_eq!(offset_of!(obc_chain_request, struct_size), 0);
        assert_eq!(offset_of!(obc_chain_request, reserved0), 4);
        assert_eq!(offset_of!(obc_chain_request, stages), 8);
        assert_eq!(offset_of!(obc_chain_request, n_stages), 16);
        assert_eq!(offset_of!(obc_chain_request, input_kind), 24);
        assert_eq!(offset_of!(obc_chain_request, input_format), 28);
        assert_eq!(offset_of!(obc_chain_request, input_width), 32);
        assert_eq!(offset_of!(obc_chain_request, input_height), 36);
        assert_eq!(offset_of!(obc_chain_request, output_format), 40);
        assert_eq!(offset_of!(obc_chain_request, output_width), 44);
        assert_eq!(offset_of!(obc_chain_request, output_height), 48);
        assert_eq!(offset_of!(obc_chain_request, matrix), 52);
        assert_eq!(offset_of!(obc_chain_request, chroma_siting), 56);
        assert_eq!(offset_of!(obc_chain_request, full_range), 60);
        assert_eq!(offset_of!(obc_chain_request, reserved1), 61);
        assert_eq!(offset_of!(obc_chain_request, cuda), 64);
        assert_eq!(offset_of!(obc_chain_request, params), 80);
        assert_eq!(offset_of!(obc_chain_request, n_params), 88);
        assert_eq!(offset_of!(obc_chain_request, background), 96);

        assert_eq!(
            (size_of::<obc_load_report>(), align_of::<obc_load_report>()),
            (24, 8)
        );
        assert_eq!(offset_of!(obc_load_report, struct_size), 0);
        assert_eq!(offset_of!(obc_load_report, load_ms), 4);
        assert_eq!(offset_of!(obc_load_report, warmup_ms), 8);
        assert_eq!(offset_of!(obc_load_report, reused_model), 12);
        assert_eq!(offset_of!(obc_load_report, reserved0), 13);
        // Appended in 1.5, after the whole of the 1.0 structure.
        assert_eq!(offset_of!(obc_load_report, notice), 16);
        assert_eq!(
            OBC_LOAD_REPORT_SIZE_1_0 as usize,
            offset_of!(obc_load_report, notice)
        );

        assert_eq!((size_of::<obc_timing>(), align_of::<obc_timing>()), (16, 8));
        assert_eq!(offset_of!(obc_timing, step), 0);
        assert_eq!(offset_of!(obc_timing, ms), 8);
        assert_eq!(offset_of!(obc_timing, reserved0), 12);

        assert_eq!(
            (
                size_of::<obc_status_value>(),
                align_of::<obc_status_value>()
            ),
            (32, 8)
        );
        assert_eq!(offset_of!(obc_status_value, stage), 0);
        assert_eq!(offset_of!(obc_status_value, name), 8);
        assert_eq!(offset_of!(obc_status_value, value), 16);
        assert_eq!(offset_of!(obc_status_value, reserved0), 24);
        assert_eq!(offset_of!(obc_status_value, reserved1), 28);

        assert_eq!(
            (
                size_of::<obc_module_vtable>(),
                align_of::<obc_module_vtable>()
            ),
            (8 + 20 * 8, 8)
        );
        assert_eq!(offset_of!(obc_module_vtable, struct_size), 0);
        assert_eq!(offset_of!(obc_module_vtable, abi_version), 4);
        assert_eq!(offset_of!(obc_module_vtable, init), 8);
        assert_eq!(offset_of!(obc_module_vtable, probe), 16);
        assert_eq!(offset_of!(obc_module_vtable, describe_settings), 24);
        assert_eq!(offset_of!(obc_module_vtable, instance_create), 32);
        assert_eq!(offset_of!(obc_module_vtable, instance_destroy), 40);
        assert_eq!(offset_of!(obc_module_vtable, instance_load), 48);
        assert_eq!(offset_of!(obc_module_vtable, instance_process), 56);
        assert_eq!(offset_of!(obc_module_vtable, instance_reset_temporal), 64);
        assert_eq!(offset_of!(obc_module_vtable, instance_unload), 72);
        assert_eq!(offset_of!(obc_module_vtable, instance_timings), 80);
        assert_eq!(offset_of!(obc_module_vtable, instance_park), 88);
        assert_eq!(offset_of!(obc_module_vtable, instance_unpark), 96);
        assert_eq!(offset_of!(obc_module_vtable, last_error), 104);
        assert_eq!(offset_of!(obc_module_vtable, instance_last_error), 112);
        assert_eq!(offset_of!(obc_module_vtable, destroy), 120);
        assert_eq!(offset_of!(obc_module_vtable, check), 128);
        // ABI 1.2.
        assert_eq!(offset_of!(obc_module_vtable, audio_instance_load), 136);
        assert_eq!(offset_of!(obc_module_vtable, audio_instance_process), 144);
        // ABI 1.3.
        assert_eq!(offset_of!(obc_module_vtable, instance_status_values), 152);
        // ABI 1.4.
        assert_eq!(offset_of!(obc_module_vtable, instance_set_background), 160);

        assert_eq!(
            (
                size_of::<obc_audio_buffer>(),
                align_of::<obc_audio_buffer>()
            ),
            (104, 8)
        );
        assert_eq!(offset_of!(obc_audio_buffer, struct_size), 0);
        assert_eq!(offset_of!(obc_audio_buffer, format), 4);
        assert_eq!(offset_of!(obc_audio_buffer, sample_rate), 8);
        assert_eq!(offset_of!(obc_audio_buffer, channels), 12);
        assert_eq!(offset_of!(obc_audio_buffer, n_samples), 16);
        assert_eq!(offset_of!(obc_audio_buffer, reserved0), 20);
        assert_eq!(offset_of!(obc_audio_buffer, planes), 24);
        assert_eq!(offset_of!(obc_audio_buffer, pts_ns), 88);
        assert_eq!(offset_of!(obc_audio_buffer, flags), 96);
        assert_eq!(offset_of!(obc_audio_buffer, voice_probability), 100);

        assert_eq!(
            (
                size_of::<obc_audio_request>(),
                align_of::<obc_audio_request>()
            ),
            (56, 8)
        );
        assert_eq!(offset_of!(obc_audio_request, struct_size), 0);
        assert_eq!(offset_of!(obc_audio_request, reserved0), 4);
        assert_eq!(offset_of!(obc_audio_request, stages), 8);
        assert_eq!(offset_of!(obc_audio_request, n_stages), 16);
        assert_eq!(offset_of!(obc_audio_request, sample_rate), 24);
        assert_eq!(offset_of!(obc_audio_request, channels_in), 28);
        assert_eq!(offset_of!(obc_audio_request, channels_out), 32);
        assert_eq!(offset_of!(obc_audio_request, frame_samples), 36);
        assert_eq!(offset_of!(obc_audio_request, params), 40);
        assert_eq!(offset_of!(obc_audio_request, n_params), 48);

        assert_eq!(
            (
                size_of::<obc_audio_load_report>(),
                align_of::<obc_audio_load_report>()
            ),
            (24, 4)
        );
        assert_eq!(offset_of!(obc_audio_load_report, struct_size), 0);
        assert_eq!(offset_of!(obc_audio_load_report, load_ms), 4);
        assert_eq!(offset_of!(obc_audio_load_report, warmup_ms), 8);
        assert_eq!(offset_of!(obc_audio_load_report, frame_samples), 12);
        assert_eq!(offset_of!(obc_audio_load_report, latency_samples), 16);
        assert_eq!(offset_of!(obc_audio_load_report, reused_model), 20);
        assert_eq!(offset_of!(obc_audio_load_report, reserved0), 21);
    }

    /// The base sizes are the sizes of the structures in the minor version
    /// that introduced them, and the vtable sizes end at the last entry of
    /// each minor version: fixed for the life of major 1.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn base_sizes_are_pinned() {
        let entry = size_of::<usize>();
        assert_eq!(OBC_HOST_SIZE_1_0 as usize, size_of::<obc_host>());
        assert_eq!(OBC_FRAME_SIZE_1_0 as usize, size_of::<obc_frame>());
        assert_eq!(
            OBC_CHAIN_REQUEST_SIZE_1_0 as usize,
            size_of::<obc_chain_request>()
        );
        assert_eq!(
            OBC_LOAD_REPORT_SIZE_1_0 as usize,
            offset_of!(obc_load_report, notice)
        );
        assert_eq!(
            OBC_LOAD_REPORT_SIZE_1_5 as usize,
            size_of::<obc_load_report>()
        );
        assert_eq!(
            OBC_AUDIO_BUFFER_SIZE_1_2 as usize,
            size_of::<obc_audio_buffer>()
        );
        assert_eq!(
            OBC_AUDIO_REQUEST_SIZE_1_2 as usize,
            size_of::<obc_audio_request>()
        );
        assert_eq!(
            OBC_AUDIO_LOAD_REPORT_SIZE_1_2 as usize,
            size_of::<obc_audio_load_report>()
        );
        assert_eq!(
            OBC_MODULE_VTABLE_SIZE_1_0 as usize,
            offset_of!(obc_module_vtable, check) + entry
        );
        assert_eq!(
            OBC_MODULE_VTABLE_SIZE_1_2 as usize,
            offset_of!(obc_module_vtable, audio_instance_process) + entry
        );
        assert_eq!(
            OBC_MODULE_VTABLE_SIZE_1_3 as usize,
            offset_of!(obc_module_vtable, instance_status_values) + entry
        );
        assert_eq!(
            OBC_MODULE_VTABLE_SIZE_1_4 as usize,
            offset_of!(obc_module_vtable, instance_set_background) + entry
        );
        assert_eq!(
            OBC_MODULE_VTABLE_SIZE_1_4 as usize,
            size_of::<obc_module_vtable>()
        );
    }

    #[test]
    fn names_are_pinned() {
        assert_eq!(OBC_STAGE_BACKGROUND, "background");
        assert_eq!(
            [
                OBC_BACKGROUND_PARAM_MODE,
                OBC_BACKGROUND_MODE_BLUR,
                OBC_BACKGROUND_MODE_REPLACE,
                OBC_BACKGROUND_PARAM_BLUR_STRENGTH
            ],
            ["mode", "blur", "replace", "blur_strength"]
        );
        assert_eq!(
            [
                OBC_PRIVATE_STAGE_PREFIX,
                OBC_PRIVATE_GROUP_PREFIX,
                OBC_RESERVED_CONFIG_PREFIX,
                OBC_HOST_CONFIG_KEY,
                OBC_RESERVED_VALUE_PREFIX
            ],
            ["x-", "x_", "_", "_host", "obc."]
        );
        assert!(OBC_WELL_KNOWN_STAGES.contains(&OBC_STAGE_BACKGROUND));
        assert!(OBC_WELL_KNOWN_VIDEO_GROUPS.contains(&"background"));
        assert!(OBC_WELL_KNOWN_AUDIO_GROUPS.contains(&"input"));
    }

    #[test]
    fn codes_are_pinned() {
        assert_eq!(
            [OBC_FRAME_CUDA, OBC_FRAME_HOST, OBC_FRAME_DMABUF],
            [1, 2, 3]
        );
        assert_eq!(
            [
                OBC_FORMAT_NV12,
                OBC_FORMAT_I420,
                OBC_FORMAT_BGRA,
                OBC_FORMAT_RGBA,
                OBC_FORMAT_RGB_F32_PLANAR,
                OBC_FORMAT_BGR
            ],
            [1, 2, 3, 4, 5, 6]
        );
        assert_eq!(
            [
                OBC_OK,
                OBC_ERR_INVALID,
                OBC_ERR_UNSUPPORTED,
                OBC_ERR_UNAVAILABLE,
                OBC_ERR_FAILED,
                OBC_ERR_STATE,
                OBC_ERR_PANIC,
                OBC_ERR_STAGE
            ],
            [0, 1, 2, 3, 4, 5, 6, 7]
        );
        assert_eq!(OBC_FRAME_FLAG_NEW_SOURCE, 1);
        assert_eq!(OBC_FRAME_FLAG_REPEAT, 2);
        assert_eq!(OBC_FRAME_FLAGS_1_0, 1);
        assert_eq!(OBC_FRAME_FLAGS_ALL, 3);
        assert_eq!(OBC_AUDIO_F32_PLANAR, 1);
        assert_eq!(
            [
                OBC_AUDIO_FLAG_NEW_SOURCE,
                OBC_AUDIO_FLAG_DISCONT,
                OBC_AUDIO_FLAGS_ALL
            ],
            [1, 2, 3]
        );
        assert_eq!(OBC_MAX_AUDIO_CHANNELS, 8);
        assert_eq!(
            [
                OBC_VALUE_BOOL,
                OBC_VALUE_INT,
                OBC_VALUE_FLOAT,
                OBC_VALUE_STRING
            ],
            [1, 2, 3, 4]
        );
    }
}
