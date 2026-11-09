//! The daemon binary as a process, without hardware: a private D-Bus daemon
//! stands in for the session bus. Each case ends before the daemon touches
//! a device (it requests its bus name first).

mod common;

use std::process::Stdio;
use std::time::Duration;

use common::{DaemonEnv, Guard, PrivateBus};

/// Runs `cmd` to completion within `limit`; its code and standard error.
fn run(mut cmd: std::process::Command, limit: Duration) -> (Option<i32>, String) {
    let child = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn omarchy-broadcastd");
    let mut g = Guard::new(child, "omarchy-broadcastd", Duration::from_secs(2));
    let mut stderr = g.child.as_mut().unwrap().stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut s);
        s
    });
    let code = g.wait_exit(limit);
    drop(g);
    (code, reader.join().unwrap())
}

#[test]
fn a_second_instance_exits_with_its_own_status_before_touching_a_device() {
    let dir = tempfile::tempdir().unwrap();
    let bus = PrivateBus::start(dir.path());
    let env = DaemonEnv::new(dir.path(), &bus);
    // Should the name check ever fail, no real loopback is written to.
    std::fs::write(
        env.config_file(),
        "[loopback]\ndevice = \"/dev/obc-test-no-such-loopback\"\n",
    )
    .unwrap();
    // Another process owns the name: the daemon is "already running".
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let conn = rt.block_on(env.connect());
    rt.block_on(conn.request_name(obc_ipc::BUS_NAME)).unwrap();

    let (code, stderr) = run(env.command(), Duration::from_secs(10));
    assert_eq!(code, Some(3), "exit status; stderr: {stderr}");
    assert!(
        stderr.contains("already owned") && stderr.contains("already running"),
        "{stderr}"
    );
    // It stopped at the name: no device was looked at.
    assert!(!stderr.contains("virtual camera"), "{stderr}");
    assert!(!env.state_file().exists());
}

#[test]
fn without_a_session_bus_it_fails() {
    let dir = tempfile::tempdir().unwrap();
    // A short socket path: the bus is missing, not unaddressable.
    let sockets = common::socket_dir();
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_omarchy-broadcastd"));
    cmd.env(
        "DBUS_SESSION_BUS_ADDRESS",
        format!("unix:path={}", sockets.path().join("no-bus").display()),
    )
    .env("XDG_CONFIG_HOME", dir.path())
    .env("XDG_STATE_HOME", dir.path());
    let (code, stderr) = run(cmd, Duration::from_secs(10));
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("D-Bus"), "{stderr}");
}

#[test]
fn usage() {
    let help = std::process::Command::new(env!("CARGO_BIN_EXE_omarchy-broadcastd"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    for flag in ["--config", "--state", "--verbose"] {
        assert!(text.contains(flag), "{text}");
    }
    let bad = std::process::Command::new(env!("CARGO_BIN_EXE_omarchy-broadcastd"))
        .arg("--bogus")
        .output()
        .unwrap();
    assert_eq!(bad.status.code(), Some(2));
}
