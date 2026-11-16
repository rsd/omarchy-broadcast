//! Log lines to the daemon's log, through the host's `log` callback.
//!
//! ```
//! obc_module_sdk::log::info!("loaded {} stages", 2);
//! obc_module_sdk::log::log(obc_module_sdk::log::Level::Warn, "no model found");
//! ```
//!
//! Lines logged before the daemon called the module's entry point, or
//! when the daemon gave no callback, are dropped. A line is one message
//! without a trailing newline; a NUL byte in it is replaced.

use std::ffi::{CString, c_char, c_void};
use std::sync::RwLock;

use obc_module_abi::{
    OBC_LOG_DEBUG, OBC_LOG_ERROR, OBC_LOG_INFO, OBC_LOG_TRACE, OBC_LOG_WARN, obc_host,
    obc_log_level,
};

/// A log level.
///
/// A later release may add levels: a `match` on it has a catch-all arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Level {
    /// An error the user should see.
    Error,
    /// Something unexpected that does not stop the module.
    Warn,
    /// A normal event.
    Info,
    /// Detail for debugging.
    Debug,
    /// Very verbose detail.
    Trace,
}

impl Level {
    /// The `OBC_LOG_*` code.
    pub fn to_raw(self) -> obc_log_level {
        match self {
            Level::Error => OBC_LOG_ERROR,
            Level::Warn => OBC_LOG_WARN,
            Level::Info => OBC_LOG_INFO,
            Level::Debug => OBC_LOG_DEBUG,
            Level::Trace => OBC_LOG_TRACE,
        }
    }
}

type LogFn = unsafe extern "C" fn(user: *mut c_void, level: obc_log_level, msg: *const c_char);

#[derive(Clone, Copy)]
struct HostLog {
    user: *mut c_void,
    log: LogFn,
}

// SAFETY: the ABI makes the host's log callback, with its `user` pointer,
// callable from any thread.
unsafe impl Send for HostLog {}
// SAFETY: as above.
unsafe impl Sync for HostLog {}

static HOST_LOG: RwLock<Option<HostLog>> = RwLock::new(None);

/// Keeps the host's log callback (called by the entry point).
pub(crate) fn set_host(host: &obc_host) {
    let log = host.log.map(|log| HostLog {
        user: host.user,
        log,
    });
    *HOST_LOG.write().unwrap_or_else(|e| e.into_inner()) = log;
}

/// A C string of `s`, NUL bytes replaced.
pub(crate) fn c_string(s: &str) -> CString {
    CString::new(s.replace('\0', "\u{fffd}")).expect("no NUL bytes left")
}

/// Writes `msg` to the daemon's log at `level`.
pub fn log(level: Level, msg: &str) {
    let host = *HOST_LOG.read().unwrap_or_else(|e| e.into_inner());
    if let Some(host) = host {
        let msg = c_string(msg);
        // SAFETY: the host keeps the callback and `user` valid until the
        // library is unloaded; `msg` lives across the call.
        unsafe { (host.log)(host.user, level.to_raw(), msg.as_ptr()) };
    }
}

#[doc(hidden)]
#[macro_export]
macro_rules! __obc_log {
    ($level:expr, $($arg:tt)+) => {
        $crate::log::log($level, &::std::format!($($arg)+))
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __obc_log_error {
    ($($arg:tt)+) => { $crate::__obc_log!($crate::log::Level::Error, $($arg)+) };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __obc_log_warn {
    ($($arg:tt)+) => { $crate::__obc_log!($crate::log::Level::Warn, $($arg)+) };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __obc_log_info {
    ($($arg:tt)+) => { $crate::__obc_log!($crate::log::Level::Info, $($arg)+) };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __obc_log_debug {
    ($($arg:tt)+) => { $crate::__obc_log!($crate::log::Level::Debug, $($arg)+) };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __obc_log_trace {
    ($($arg:tt)+) => { $crate::__obc_log!($crate::log::Level::Trace, $($arg)+) };
}

/// Logs a formatted line at [`Level::Debug`]: `log::debug!("{x}")`.
#[doc(inline)]
pub use crate::__obc_log_debug as debug;
/// Logs a formatted line at [`Level::Error`]: `log::error!("{x}")`.
#[doc(inline)]
pub use crate::__obc_log_error as error;
/// Logs a formatted line at [`Level::Info`]: `log::info!("{x}")`.
#[doc(inline)]
pub use crate::__obc_log_info as info;
/// Logs a formatted line at [`Level::Trace`]: `log::trace!("{x}")`.
#[doc(inline)]
pub use crate::__obc_log_trace as trace;
/// Logs a formatted line at [`Level::Warn`]: `log::warn!("{x}")`.
#[doc(inline)]
pub use crate::__obc_log_warn as warn;
