//! `omarchy-broadcastd`: the headless daemon that drives the virtual camera
//! (`docs/adr/0011-daemon-lifecycle-and-dbus-api.md`; the client reference
//! is `docs/dbus-api.md`).
//!
//! Threads and their roles:
//!
//! * the **Tokio runtime** (one thread): the D-Bus connection, the
//!   `br.com.dias.OmarchyBroadcast1` object ([`dbus::Server`]), the task that
//!   emits `PropertiesChanged` and `Stats` ([`dbus::run_publisher`]), and the
//!   SIGHUP / SIGTERM / SIGINT handlers;
//! * the **controller** ([`controller::Controller`], a std thread): owns
//!   every piece of media state through a [`backend::MediaBackend`] (the
//!   persistent loopback writer, the CUDA session while active), the config
//!   and the runtime state, and runs the lifecycle state machine. Everything
//!   else talks to it through [`controller::Command`]s; it publishes the
//!   property snapshot the D-Bus side serves;
//! * the **consumer watcher** ([`consumers::ConsumerWatcher`]): inotify on
//!   the loopback node, then a `/proc/<pid>/fd` count of the processes
//!   holding it;
//! * the **camera watcher** ([`hotplug::CameraWatcher`]): inotify on `/dev`
//!   for `video*` nodes appearing and disappearing;
//! * while a preview lease is held, the **preview encoder**
//!   (`obc_pipeline::preview`), fed by the writer's tap; its frames reach
//!   the D-Bus side (`PreviewFrame`) through [`controller::Published`],
//!   without the controller.
//!
//! The camera settings channel (revision 7) is modelled, without any
//! device, in [`settings`]; the controller feeds it what the camera's node
//! reports and publishes its tree.
//!
//! The lifecycle logic takes the current time as an argument, so its tests
//! drive it with a fake backend and explicit instants.

pub mod backend;
pub mod camera;
pub mod consumers;
pub mod controller;
pub mod daemon;
pub mod dbus;
pub mod gpu_cache;
pub mod hotplug;
pub mod inotify;
pub mod log;
pub mod preview;
pub mod settings;
