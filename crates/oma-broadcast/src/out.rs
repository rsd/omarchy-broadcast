//! Writing to stdout, and leaving quietly when nobody reads it any more.
//!
//! Rust starts with `SIGPIPE` ignored, so a write to a closed pipe fails with
//! `EPIPE`, which ends the process with status 0 and no message. `watch` can
//! sit idle for a long time between lines, so [`closed`] also watches the
//! stdout descriptor itself: a pipe whose reader has gone reports `POLLERR`
//! at once, and `oma-broadcast watch --json | head -1` ends as soon as `head`
//! does.

use std::io::{self, Write};
use std::process;

use crate::bus::{EXIT_FAILURE, EXIT_OK};

/// Writes `text` to stdout and flushes it. A closed pipe exits with status 0;
/// any other write error exits with [`EXIT_FAILURE`].
pub fn print(text: &str) {
    let mut stdout = io::stdout().lock();
    let result = stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush());
    if let Err(e) = result {
        if e.kind() == io::ErrorKind::BrokenPipe {
            process::exit(i32::from(EXIT_OK));
        }
        eprintln!("oma-broadcast: cannot write to stdout: {e}");
        process::exit(i32::from(EXIT_FAILURE));
    }
}

/// Writes one line (a newline is added) and flushes it.
pub fn line(text: &str) {
    let mut s = String::with_capacity(text.len() + 1);
    s.push_str(text);
    s.push('\n');
    print(&s);
}

/// Resolves when stdout reports an error or hang-up condition (the reader
/// of a pipe or socket has gone). Never resolves for a terminal, a regular
/// file or `/dev/null`.
pub async fn closed() {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    std::thread::spawn(move || {
        loop {
            let mut fds = libc::pollfd {
                fd: libc::STDOUT_FILENO,
                // No events requested: POLLERR, POLLHUP and POLLNVAL are
                // always reported.
                events: 0,
                revents: 0,
            };
            // SAFETY: `fds` is one valid `pollfd` that outlives the call.
            let rc = unsafe { libc::poll(&mut fds, 1, -1) };
            if rc < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
            if fds.revents & (libc::POLLERR | libc::POLLHUP) != 0 {
                let _ = tx.send(());
                return;
            }
            if fds.revents & libc::POLLNVAL != 0 {
                return;
            }
        }
    });
    if rx.await.is_err() {
        // The watcher gave up (stdout is not pollable): never resolve.
        std::future::pending::<()>().await;
    }
}
