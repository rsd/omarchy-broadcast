//! The audio worker of Omarchy Broadcast (see the library documentation).

use std::process::ExitCode;

fn main() -> ExitCode {
    match omarchy_broadcast_audiod::Options::parse(std::env::args().skip(1)) {
        Ok(opts) => omarchy_broadcast_audiod::run(opts),
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
    }
}
