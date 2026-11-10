//! The stable C ABI between omarchy-broadcast and its modules.
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
//! # Conventions
//!
//! * Every structure that may grow in a later minor version starts with a
//!   `uint32_t struct_size`, set by whoever fills it in to the size it was
//!   built with. Fields are only ever appended after the current end of a
//!   structure, and padding is spelled out as `reserved` fields that must be
//!   zero, so a reader can tell which fields are present from
//!   `struct_size` alone. A reader accepts any `struct_size` at least the
//!   structure's size in ABI 1.0 (its size in this version: nothing has been
//!   appended yet), larger or smaller than its own, and reads a field only
//!   when it fits in `struct_size`; it rejects a smaller one
//!   ([`OBC_ERR_INVALID`]). A writer writes only the fields that fit in the
//!   reader's `struct_size` when the reader set it (an output structure).
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
//!   it. The host refuses a vtable that lacks a required one.
//!
//! # Threading
//!
//! The host serialises the calls on one module object, and the calls on one
//! instance (they come from the streaming thread). Calls on the module and
//! on its instances may run concurrently on different threads, as may calls
//! on different instances. [`obc_host::log`] may be called from any thread.
//!
//! # CUDA frames
//!
//! For [`OBC_FRAME_CUDA`] frames the host makes [`obc_cuda::context`]
//! current on the calling thread around every instance call. A module never
//! creates a CUDA context of its own, and never makes one current.

// The names are the C names of the header, as in a `-sys` crate.
#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_void};
use std::marker::{PhantomData, PhantomPinned};

/// The major version of this ABI. A change of the major version is
/// incompatible: the host refuses a module built for another major version.
pub const OBC_MODULE_ABI_MAJOR: u32 = 1;

/// The minor version of this ABI. A minor version only adds (structure
/// fields at the end, vtable entries at the end, constants): the host
/// accepts a module built for its own or an older minor version.
pub const OBC_MODULE_ABI_MINOR: u32 = 0;

/// The packed ABI version this crate describes: `major << 16 | minor`
/// ([`obc_module_abi_pack`]).
pub const OBC_MODULE_ABI_VERSION: u32 = (OBC_MODULE_ABI_MAJOR << 16) | OBC_MODULE_ABI_MINOR;

/// The name of the exported [`obc_module_abi_version_fn`].
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
/// chain is intact. Only `instance_load` and `instance_process` return it,
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
    /// `OBC_FRAME_FLAG_*` bits; the others are zero.
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

/// What `instance_load` reports. The host sets `struct_size` and zeroes the
/// rest; the module fills in the fields that fit in `struct_size`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct obc_load_report {
    /// `sizeof(obc_load_report)` as the host was built.
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

// ---------------------------------------------------------------------------
// The vtable
// ---------------------------------------------------------------------------

/// The module's entry points, returned by [`obc_module_entry_fn`].
///
/// Every entry is declared nullable; those marked required must be set. A
/// later minor version only appends entries, so a host reads an entry only
/// when it fits in `struct_size`.
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
    /// Required. Loads a chain of the module's stages. A load on a loaded
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
    /// Required. Enqueues one frame through the loaded chain. `out` equals
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
    /// Nullable (null: nothing to do). The stream is about to stop for a
    /// while: release what can be rebuilt quickly.
    pub instance_park: Option<unsafe extern "C" fn(instance: *mut obc_instance) -> obc_status>,
    /// Nullable (null: nothing to do). The stream resumes after a park.
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
}

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
        assert_eq!(OBC_MODULE_ABI_VERSION, 0x0001_0000);
        assert_eq!(obc_module_abi_pack(1, 0), OBC_MODULE_ABI_VERSION);
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
            (16, 4)
        );
        assert_eq!(offset_of!(obc_load_report, struct_size), 0);
        assert_eq!(offset_of!(obc_load_report, load_ms), 4);
        assert_eq!(offset_of!(obc_load_report, warmup_ms), 8);
        assert_eq!(offset_of!(obc_load_report, reused_model), 12);
        assert_eq!(offset_of!(obc_load_report, reserved0), 13);

        assert_eq!((size_of::<obc_timing>(), align_of::<obc_timing>()), (16, 8));
        assert_eq!(offset_of!(obc_timing, step), 0);
        assert_eq!(offset_of!(obc_timing, ms), 8);
        assert_eq!(offset_of!(obc_timing, reserved0), 12);

        assert_eq!(
            (
                size_of::<obc_module_vtable>(),
                align_of::<obc_module_vtable>()
            ),
            (8 + 16 * 8, 8)
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
