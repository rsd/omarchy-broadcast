//! `MAXINE_AR_ROOT` takes precedence over the configured root. One test in
//! its own binary: it sets the variable for the whole process.

mod support;

use obc_backend::Availability;

#[test]
fn the_environment_root_is_probed_before_the_configured_one() {
    let empty = tempfile::tempdir().unwrap();
    // SAFETY: the only test of this binary, before any thread of its own
    // reads the environment (the module reads it in `init`, below).
    unsafe { std::env::set_var("MAXINE_AR_ROOT", empty.path()) };
    let dir = tempfile::tempdir().unwrap();
    // The configured root is the packaged SDK's, if installed: the
    // environment's empty directory still wins.
    let module = support::open(
        dir.path(),
        serde_json::json!({"sdk_root": "/opt/nvidia/maxine/arsdk"}),
    );
    let probe = module.probe().unwrap();
    assert_eq!(probe.len(), 2);
    for (_, a) in &probe {
        assert_eq!(
            *a,
            Availability::NotInstalled("Maxine AR SDK not installed".into())
        );
    }
}
