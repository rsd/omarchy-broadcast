//! The host side of omarchy-broadcast modules: finding, loading and
//! arbitrating them, and running their stages through the in-process seams
//! of `obc-backend`.
//!
//! A module is a manifest ([`manifest`]) in a `modules.d` directory plus a
//! shared library that exports the C ABI of `obc-module-abi`. The pieces:
//!
//! * [`discovery`]: scans the manifest directories ([`discovery::default_dirs`],
//!   [`discovery::scan`]); reads manifests only, never a library;
//! * [`library`]: opens a library and checks its ABI version before
//!   anything else of it runs ([`library::Library::open`]);
//! * [`module`]: the safe handles over a loaded module ([`Module`]: probe,
//!   settings, instances) and its instances ([`module::Instance`]);
//! * [`audio`]: the instances of an `audio-filter` module
//!   ([`AudioInstance`], ABI 1.2), which only a host of
//!   [`LoadKinds::Audio`] (the audio worker) opens;
//! * [`settings`]: the typed, validated model of a module's settings
//!   description;
//! * [`adapters`]: [`ModuleBackendFactory`], [`ModuleEnhancerFactory`] and
//!   [`ModuleChainFactory`], the `obc-backend` seams over a module, for the
//!   effect element, the ingest's enhance stage and other chains;
//! * [`registry`]: the [`Registry`] of discovered modules: enabled or not,
//!   their status, who provides each stage (rank, pin, profile, the
//!   phase-1 placement rule) and the stage order;
//! * [`crash`]: the [`CrashLedger`] that disables a module which took the
//!   daemon down twice in a row.
//!
//! # A failing module stays contained
//!
//! Nothing a module does at discovery, load or probe time is an error of
//! the host: a broken manifest, a missing library, another ABI version, a
//! failing `init` or probe each become the module's status with a reason.
//! A panic in the module's code is caught by the module (the SDK does it)
//! and returned as a failure of that call ([`module::Status::Panic`]); the
//! instance is then unloaded. What the host cannot contain is a crash of
//! the module's native code, which takes the process down; the crash
//! ledger keeps such a module from doing it more than twice in a row.

pub mod adapters;
pub mod audio;
pub mod crash;
pub mod discovery;
pub mod library;
pub mod manifest;
pub mod module;
pub mod registry;
pub mod settings;

pub use adapters::{
    BACKGROUND_BLUR_STRENGTH, BACKGROUND_MODE, ModuleBackendFactory, ModuleChainFactory,
    ModuleEnhancerFactory,
};
pub use audio::{AudioBlock, AudioInstance, AudioLoadOutcome, AudioSpec, common_block_formats};
pub use crash::{CrashLedger, REASON_CRASHED};
pub use discovery::{Discovery, ModuleEntry, default_dirs, scan};
pub use library::{LogLevel, LogSink, stderr_log};
pub use manifest::{
    AudioDecl, Device, Format, Frames, LoadKinds, Manifest, ModuleKind, Requirement, Slot,
    StageDecl,
};
pub use module::{Instance, Module, ModuleError, Status};
pub use registry::{
    ModuleInfo, ModuleStatus, Profile, Provider, Registry, StageInfo, StageResolution,
};
pub use settings::{SettingDesc, SettingType, SettingsDescription, SettingsGroup};
