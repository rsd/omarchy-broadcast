//! Helpers shared by the daemon's process-level tests: a private D-Bus
//! daemon, child-process guards and the daemon's environment.

#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The D-Bus daemon these tests run their own bus with.
pub const DBUS_DAEMON: &str = "/usr/bin/dbus-daemon";

/// A child process killed (SIGTERM, then SIGKILL after `grace`) and reaped
/// when dropped, also when the test fails.
pub struct Guard {
    pub child: Option<Child>,
    pub what: &'static str,
    pub grace: Duration,
}

impl Guard {
    pub fn new(child: Child, what: &'static str, grace: Duration) -> Guard {
        Guard {
            child: Some(child),
            what,
            grace,
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.as_ref().map_or(0, Child::id)
    }

    pub fn signal(&self, sig: i32) {
        if let Some(c) = &self.child {
            // SAFETY: plain kill(2) on our own child's pid.
            unsafe {
                libc::kill(c.id() as i32, sig);
            }
        }
    }

    /// Waits up to `limit` for the child to exit; its status code.
    pub fn wait_exit(&mut self, limit: Duration) -> Option<i32> {
        let deadline = Instant::now() + limit;
        let child = self.child.as_mut()?;
        loop {
            if let Ok(Some(st)) = child.try_wait() {
                self.child = None;
                return Some(st.code().unwrap_or(-1));
            }
            if Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// SIGTERM, then the exit status (SIGKILL after the grace).
    pub fn terminate(&mut self) -> Option<i32> {
        self.signal(libc::SIGTERM);
        let code = self.wait_exit(self.grace);
        if code.is_none()
            && let Some(mut c) = self.child.take()
        {
            eprintln!("{} did not exit after SIGTERM: killing it", self.what);
            let _ = c.kill();
            let _ = c.wait();
        }
        code
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if self.child.is_some() {
            self.terminate();
        }
    }
}

/// A private session bus: `dbus-daemon --nofork` on a socket in a
/// temporary directory, with no service activation. Stopped when dropped.
pub struct PrivateBus {
    pub address: String,
    _guard: Guard,
    /// Where the socket is (after `_guard`: removed once the bus stopped).
    _socket_dir: tempfile::TempDir,
}

/// A new directory with a short path for a bus socket, removed when
/// dropped. A unix socket path is limited to 108 bytes, which a long
/// `TMPDIR` or target directory exceeds: `$XDG_RUNTIME_DIR` when it is
/// set, short and writable, else `/tmp`.
pub fn socket_dir() -> tempfile::TempDir {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|d| d.is_absolute() && d.as_os_str().len() <= 64);
    runtime
        .into_iter()
        .chain([PathBuf::from("/tmp")])
        .find_map(|base| {
            tempfile::Builder::new()
                .prefix("obc-")
                .tempdir_in(base)
                .ok()
        })
        .expect("no writable directory for the test bus socket ($XDG_RUNTIME_DIR, /tmp)")
}

impl PrivateBus {
    /// Starts the bus (its config in `dir`, its socket in a [`socket_dir`]);
    /// fails the test, naming the binary, without it.
    pub fn start(dir: &Path) -> PrivateBus {
        assert!(
            Path::new(DBUS_DAEMON).exists(),
            "this test needs {DBUS_DAEMON} (the reference D-Bus daemon, package `dbus`) to run a \
             private session bus"
        );
        let socket_dir = socket_dir();
        let config = dir.join("bus.conf");
        std::fs::write(
            &config,
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:dir={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
                socket_dir.path().display()
            ),
        )
        .unwrap();
        let mut child = Command::new(DBUS_DAEMON)
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--print-address=1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap_or_else(|e| panic!("cannot start {DBUS_DAEMON}: {e}"));
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let address = line.trim().to_string();
        assert!(
            address.starts_with("unix:"),
            "{DBUS_DAEMON} printed no address: {line:?}"
        );
        PrivateBus {
            address,
            _guard: Guard::new(child, "dbus-daemon", Duration::from_secs(2)),
            _socket_dir: socket_dir,
        }
    }
}

/// The environment of a daemon under test: a private bus, and config and
/// state homes in `dir`.
pub struct DaemonEnv {
    pub bus_address: String,
    pub config_home: PathBuf,
    pub state_home: PathBuf,
}

impl DaemonEnv {
    pub fn new(dir: &Path, bus: &PrivateBus) -> DaemonEnv {
        let env = DaemonEnv {
            bus_address: bus.address.clone(),
            config_home: dir.join("config"),
            state_home: dir.join("state"),
        };
        std::fs::create_dir_all(env.config_home.join("oma-broadcast")).unwrap();
        std::fs::create_dir_all(&env.state_home).unwrap();
        env
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_home.join("oma-broadcast").join("config.toml")
    }

    pub fn state_file(&self) -> PathBuf {
        self.state_home.join("oma-broadcast").join("state.toml")
    }

    /// The daemon binary with this environment (and nothing else of the
    /// test's session: no session bus, no user config).
    pub fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_oma-broadcastd"));
        c.env("DBUS_SESSION_BUS_ADDRESS", &self.bus_address)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_STATE_HOME", &self.state_home)
            .stdin(Stdio::null());
        c
    }

    /// A client connection to the private bus.
    pub async fn connect(&self) -> zbus::Connection {
        zbus::connection::Builder::address(self.bus_address.as_str())
            .build()
            .await
            .expect("connect to the private bus")
    }
}
