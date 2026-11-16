//! The audio worker of Omarchy Broadcast (see the library documentation).

use std::process::ExitCode;

fn main() -> ExitCode {
    match omarchy_broadcast_audiod::Options::parse(std::env::args().skip(1)) {
        Ok(opts) => {
            omarchy_broadcast_audiod::log::set_verbose(opts.verbose);
            // SAFETY: nothing has started a thread yet (they all start in
            // `run`). This runs before the listener takes the socket systemd
            // handed over: executing again keeps descriptor 3, the
            // `LISTEN_*` variables and the PID.
            unsafe { obc_core::sdk_env::apply(&obc_core::sdk_env::AUDIOD, &mut log_sdk_env) };
            omarchy_broadcast_audiod::run(opts)
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
    }
}

/// Logs a line of the SDK environment check ([`obc_core::sdk_env`]).
fn log_sdk_env(level: obc_core::sdk_env::Level, msg: &str) {
    use obc_core::sdk_env::Level as L;
    use omarchy_broadcast_audiod::log::{Level, line};
    let level = match level {
        L::Debug => Level::Debug,
        L::Info => Level::Info,
        L::Warning => Level::Warning,
    };
    line(level, msg);
}
