//! Logging to standard error (the journal, under systemd).
//!
//! Lines carry the syslog priority prefix journald understands (`<3>` error,
//! `<4>` warning, `<6>` info, `<7>` debug) when standard error is not a
//! terminal, so `journalctl -p warning` filters them; on a terminal they are
//! plain. Debug lines are printed only with `-v`.

use std::io::IsTerminal;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

static VERBOSE: AtomicBool = AtomicBool::new(false);
static PREFIX: OnceLock<bool> = OnceLock::new();

/// Enables debug lines.
pub fn set_verbose(on: bool) {
    VERBOSE.store(on, Ordering::Relaxed);
}

pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

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
    // eprintln! (not a raw stderr handle): the test harness captures it.
    if journal {
        eprintln!("<{}>{}{msg}", level.priority(), level.word());
    } else {
        eprintln!("oma-broadcastd: {}{msg}", level.word());
    }
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
