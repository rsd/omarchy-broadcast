//! The Rust SDK for omarchy-broadcast modules.
//!
//! A module is a shared library (`crate-type = ["cdylib"]`) plus a TOML
//! manifest; the daemon loads the library at run time through the C ABI of
//! [`obc_module_abi`] (re-exported as [`abi`]). This crate wraps that ABI
//! in a safe API: implement [`Module`] and [`Instance`], then export them
//! with [`export_module!`]:
//!
//! ```
//! use obc_module_sdk::{
//!     ChainRequest, Error, Frame, Instance, LoadReport, Module, Param, ProbeReport,
//!     SettingsDescription,
//! };
//!
//! struct Hello;
//! struct HelloInstance;
//!
//! impl Module for Hello {
//!     type Instance = HelloInstance;
//!     fn init(_config: obc_module_sdk::serde_json::Value) -> Result<Self, Error> {
//!         Ok(Hello)
//!     }
//!     fn probe(&self) -> ProbeReport {
//!         ProbeReport::new().available("hello")
//!     }
//!     fn settings(&self) -> SettingsDescription {
//!         SettingsDescription::new()
//!     }
//!     fn create_instance(&self) -> Result<HelloInstance, Error> {
//!         Ok(HelloInstance)
//!     }
//! }
//!
//! impl Instance for HelloInstance {
//!     fn load(&mut self, _req: &ChainRequest<'_>) -> Result<LoadReport, Error> {
//!         Ok(LoadReport::default())
//!     }
//!     fn process(&mut self, _in: &Frame<'_>, _out: &Frame<'_>, _live: &[Param<'_>]) -> Result<(), Error> {
//!         Ok(())
//!     }
//!     fn unload(&mut self) {}
//! }
//!
//! obc_module_sdk::export_module!(Hello);
//! ```
//!
//! # What the exported entry points do
//!
//! [`export_module!`] defines the two symbols the daemon looks up,
//! `obc_module_abi_version` and `obc_module_entry`, and a static vtable
//! whose functions, for every call:
//!
//! * check the pointers and the `struct_size` of every structure they are
//!   given, and refuse a null pointer or a structure smaller than this ABI
//!   version's with [`ErrorKind::Invalid`];
//! * convert the C structures into the safe views of this crate
//!   ([`Frame`], [`ChainRequest`], [`Param`]) and the results back;
//! * catch a panic of the module's code: it becomes an
//!   [`ErrorKind::Panic`] error carrying the panic message, and never
//!   unwinds into the daemon. Do not build a module with `panic = "abort"`,
//!   or a panic takes the daemon down with it;
//! * keep every string handed back (JSON documents, error messages, timing
//!   names) alive until the next call on the same object.
//!
//! Log lines go to the daemon's log through [`log`].
//!
//! # Threading
//!
//! The daemon serialises the calls on the module and, separately, the calls
//! on each instance; the module and its instances may be called at the
//! same time from different threads. Hence `Module: Send + Sync` and
//! `Instance: Send`. State shared between the module and its instances
//! belongs in an `Arc` (with its own locking) handed to each instance.
//!
//! # Stability
//!
//! The ABI this crate wraps (ABI 1.x, [`abi`]) is not yet declared stable.
//! Until it is, it may change in breaking ways, even without a change of the
//! major version. A module built with this crate may therefore need
//! rebuilding, against the matching release of this crate, after an update
//! of omarchy-broadcast.

pub use obc_module_abi as abi;
/// Re-exported so a module needs no direct dependency to read its
/// configuration.
pub use serde_json;

mod describe;
mod error;
#[doc(hidden)]
pub mod ffi;
mod frame;
pub mod log;
mod request;

pub use describe::{
    Apply, Condition, Group, Op, ProbeReport, Role, Setting, SettingKind, SettingsDescription,
    StageProbe,
};
pub use error::{Error, ErrorKind};
pub use frame::{CudaHandles, Frame, FrameKind, PixelFormat};
pub use request::{
    ChainFrames, ChainRequest, ChromaSiting, ColourMatrix, LoadReport, Param, Value,
};

/// A module: the object the daemon creates once per loaded library (per
/// enabled module), from the module's configuration.
pub trait Module: Send + Sync + Sized + 'static {
    /// The per-stream instance type.
    type Instance: Instance;

    /// Creates the module from its configuration: the module's
    /// `[module."<id>"]` table of the daemon configuration, as JSON (an
    /// empty object when there is none). Must be cheap: no GPU work, no
    /// model loading. An [`Error`] from `serde_json` converts into an
    /// [`ErrorKind::Invalid`] error, so `serde_json::from_value(config)?`
    /// works.
    fn init(config: serde_json::Value) -> Result<Self, Error>;

    /// Which of the module's stages can run here, and why not for those
    /// that cannot. Filesystem checks only: it must not create a CUDA
    /// context, load a model or otherwise wake a suspended GPU.
    fn probe(&self) -> ProbeReport;

    /// The module's settings, shown by the daemon's clients.
    fn settings(&self) -> SettingsDescription;

    /// Checks more deeply than [`Module::probe`] that `stages` can run
    /// here, before a stream that runs them starts: the daemon calls it so
    /// that a missing runtime is told before the camera opens, not when the
    /// chain loads. It may open the module's own libraries (its runtime),
    /// but must not create a CUDA context, load a model or otherwise touch
    /// the GPU. The default finds nothing more than the probe:
    /// `Ok(())`. Fail with [`ErrorKind::Unavailable`] for a runtime that is
    /// not there.
    fn check(&self, stages: &[&str]) -> Result<(), Error> {
        let _ = stages;
        Ok(())
    }

    /// Creates an instance, which runs one chain of the module's stages for
    /// one stream. Cheap: the work happens in [`Instance::load`].
    fn create_instance(&self) -> Result<Self::Instance, Error>;
}

/// One chain of a module's stages for one stream. All calls come from the
/// stream's thread, one at a time.
pub trait Instance: Send + 'static {
    /// Loads the chain the request names ([`ChainRequest::stages`], in
    /// order) for its frames ([`ChainRequest::frames`]). A load on a loaded
    /// instance with the frames of the loaded chain is a **switch**, which
    /// may reuse models of the previous load (say so in
    /// [`LoadReport::reused_model`]); with other frames it is a
    /// **reload**.
    ///
    /// A switch that fails must leave the previous chain loaded and
    /// running as it was: the SDK keeps the instance loaded, and the next
    /// frames run the previous chain. After an [`ErrorKind::Stage`] error
    /// ([`Error::stage`]: one stage alone failed) the previous chain may
    /// have lost that stage, when the switch could not leave it as it was;
    /// the next frames then run without it. When a first load or a reload
    /// fails, or a load panics, the SDK calls [`Instance::unload`], so
    /// `unload` must cope with a partial load.
    fn load(&mut self, req: &ChainRequest<'_>) -> Result<LoadReport, Error>;

    /// Runs one frame through the loaded chain, from `input` into `output`.
    /// The two describe the same memory ([`Frame::same_memory`]) only for
    /// an in-place chain. `live` holds the per-frame parameters, which
    /// apply from this frame on. Only called after a successful load. A
    /// stage that fails on its own is an [`Error::stage`]: the instance has
    /// then dropped the stage, and the next frame runs without it. The
    /// daemon takes a stage failure of a matte stage (`background`, or a
    /// stage of the `matte` slot) as a failure of the whole chain and
    /// unloads the instance: a matte never fails open.
    fn process(
        &mut self,
        input: &Frame<'_>,
        output: &Frame<'_>,
        live: &[Param<'_>],
    ) -> Result<(), Error>;

    /// Forgets temporal state (a new source, a seek). Returns the stage ids
    /// that could not keep running and were dropped from the chain.
    fn reset_temporal(&mut self) -> Result<Vec<String>, Error> {
        Ok(Vec::new())
    }

    /// Releases what the last load loaded. The instance can be loaded
    /// again.
    fn unload(&mut self);

    /// How long each step of the last processed frame took: `(step,
    /// milliseconds)`, a step being a stage id or a step of the module's
    /// own (`convert-in`, `mask`, `composite`, `convert-out`).
    fn timings(&self) -> Vec<(String, f32)> {
        Vec::new()
    }

    /// The stream is about to stop for a while: release what can be rebuilt
    /// quickly. The default does nothing.
    fn park(&mut self) -> Result<(), Error> {
        Ok(())
    }

    /// The stream resumes after [`Instance::park`]. The default does
    /// nothing.
    fn unpark(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

/// Exports a [`Module`] type as the module of this library: defines the
/// `obc_module_abi_version` and `obc_module_entry` symbols and the vtable
/// behind them. Use it once per library, at the crate root or in any
/// module:
///
/// ```ignore
/// obc_module_sdk::export_module!(MyModule);
/// ```
#[macro_export]
macro_rules! export_module {
    ($module:ty) => {
        /// The packed ABI version this module was built against.
        #[unsafe(no_mangle)]
        pub extern "C" fn obc_module_abi_version() -> u32 {
            $crate::abi::OBC_MODULE_ABI_VERSION
        }

        /// The module's entry point: checks the host and returns the
        /// vtable.
        ///
        /// # Safety
        ///
        /// `host` is null or points to an `obc_host` that stays valid until
        /// the library is unloaded.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn obc_module_entry(
            host: *const $crate::abi::obc_host,
        ) -> *const $crate::abi::obc_module_vtable {
            static VTABLE: $crate::abi::obc_module_vtable = $crate::ffi::vtable::<$module>();
            // SAFETY: the caller's contract is the one `ffi::entry` needs.
            unsafe { $crate::ffi::entry(host, &VTABLE) }
        }

        // The exports have the types the ABI declares.
        const _: $crate::abi::obc_module_abi_version_fn = obc_module_abi_version;
        const _: $crate::abi::obc_module_entry_fn = obc_module_entry;
    };
}
