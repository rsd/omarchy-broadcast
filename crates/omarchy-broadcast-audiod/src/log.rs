//! Logging to standard error (the journal, under systemd), as the video
//! daemon does: lines carry the syslog priority prefix journald understands
//! (`<3>` error, `<4>` warning, `<6>` info, `<7>` debug) when standard error
//! is not a terminal. Debug lines are printed only with `-v`.

use std::io::IsTerminal;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use obc_module_host::{LogLevel, LogSink};

static VERBOSE: AtomicBool = AtomicBool::new(false);
static PREFIX: OnceLock<bool> = OnceLock::new();

/// Enables debug lines.
pub fn set_verbose(on: bool) {
    VERBOSE.store(on, Ordering::Relaxed);
}

/// Whether debug lines are printed.
pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

/// A line's level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Error,
    Warning,
    Info,
    Debug,
}

impl Level {
    fn priority(self) -> u8 {
        match self {
            Level::Error => 3,
            Level::Warning => 4,
            Level::Info => 6,
            Level::Debug => 7,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Level::Error => "error: ",
            Level::Warning => "warning: ",
            Level::Info | Level::Debug => "",
        }
    }
}

/// Writes one line at `level`.
pub fn line(level: Level, msg: &str) {
    if level == Level::Debug && !verbose() {
        return;
    }
    let journal = *PREFIX.get_or_init(|| !std::io::stderr().is_terminal());
    if journal {
        eprintln!("<{}>{}{msg}", level.priority(), level.word());
    } else {
        eprintln!("{}{msg}", level.word());
    }
}

/// The modules' log lines, as `module <id>: <line>` (debug lines only with
/// `-v`).
pub fn module_sink() -> LogSink {
    std::sync::Arc::new(|id, level, msg| {
        let level = match level {
            LogLevel::Error => Level::Error,
            LogLevel::Warn => Level::Warning,
            LogLevel::Info => Level::Info,
            _ => Level::Debug,
        };
        line(level, &format!("module {id}: {msg}"));
    })
}

#[macro_export]
macro_rules! error {
    ($($t:tt)*) => { $crate::log::line($crate::log::Level::Error, &format!($($t)*)) };
}
#[macro_export]
macro_rules! warning {
    ($($t:tt)*) => { $crate::log::line($crate::log::Level::Warning, &format!($($t)*)) };
}
#[macro_export]
macro_rules! info {
    ($($t:tt)*) => { $crate::log::line($crate::log::Level::Info, &format!($($t)*)) };
}
#[macro_export]
macro_rules! debug {
    ($($t:tt)*) => { $crate::log::line($crate::log::Level::Debug, &format!($($t)*)) };
}
