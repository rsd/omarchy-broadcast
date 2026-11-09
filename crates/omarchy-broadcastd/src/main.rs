//! `omarchy-broadcastd`: the headless daemon (see the library docs and
//! `docs/adr/0011-daemon-lifecycle-and-dbus-api.md`). Normally run as the
//! systemd user service `omarchy-broadcastd.service`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

/// The omarchy-broadcast daemon: drives the virtual camera and serves
/// br.com.dias.OmarchyBroadcast on the D-Bus session bus.
///
/// Logs go to standard error. Exit status: 0 after SIGTERM/SIGINT, 1 on a
/// failure, 2 on a usage error, 3 when the daemon is already running.
#[derive(Debug, Parser)]
#[command(name = "omarchy-broadcastd", version)]
struct Args {
    /// Config file [default: $XDG_CONFIG_HOME/omarchy-broadcast/config.toml].
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Runtime state file [default: $XDG_STATE_HOME/omarchy-broadcast/state.toml].
    #[arg(long, value_name = "PATH")]
    state: Option<PathBuf>,
    /// Log debug details.
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();
    omarchy_broadcastd::daemon::run(omarchy_broadcastd::daemon::Options {
        config: args.config,
        state: args.state,
        verbose: args.verbose,
    })
}
