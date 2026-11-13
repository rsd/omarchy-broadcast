//! The `br.com.dias.OmarchyBroadcast1` object (`docs/dbus-api.md`).
//!
//! [`Server`] answers property reads from the controller's published
//! snapshot and forwards each method to the controller, replying with its
//! result. [`run_publisher`] turns the controller's notes into
//! `PropertiesChanged` signals (one per changed property, with the value of
//! that step, so a short-lived state such as `starting` is announced too),
//! `Stats`, `PreviewFrame` and `SettingsChanged` signals (the last after
//! the `SettingsRevision` change of the same step, so a client reading the
//! property on the signal sees its revision) and `ConfigChanged` (after the
//! `ConfigSerial` change). [`run_owner_watch`] follows the
//! unique names leaving the bus, which ends their preview leases.
//!
//! A preview lease belongs to the caller's unique bus name (the message
//! header's sender). A caller that left the bus before its lease was
//! recorded would leak it (its `NameOwnerChanged` may come before the
//! lease): so once the lease is recorded, `AcquirePreview` asks the bus
//! whether the name still has an owner, and ends the lease if not. Over a
//! peer-to-peer connection (no bus, no sender) `AcquirePreview` fails.
//!
//! The media gallery (revision 15) is the object's own: [`Server::new`]
//! takes the [`MediaStore`] (or why there is none, then `GetMedia`,
//! `ImportMedia` and `RemoveMedia` answer `Failed` with that reason), its
//! calls run on the blocking pool, and `MediaSerial` is kept here, from 1,
//! raised after each import that stored a file, each removal and each
//! poster made, with its `PropertiesChanged`. `GetMedia` lists posters
//! that exist only (the listing never decodes, ADR 0032) and hands the
//! videos without one to the store's one [`PosterMaker`]; each poster it
//! makes raises `MediaSerial` (posters landing together raise it once), so
//! a client lists again and finds it. It restarts at 1 at each daemon
//! start: a client
//! compares serials within one daemon run (a new owner of the bus name is
//! a new run). A removal is checked here (a user item), then checked
//! against the configuration, cleared from the run-time background and
//! performed by the controller in one step ([`Command::ReleaseMedia`]).
//!
//! `SetBackgroundImage` of a video runs the video check here, on the
//! blocking pool, and hands its result to the controller with the
//! command: the controller's thread never runs it (it can take seconds).
//! `GetConvertCommand` (revision 19) runs the same check there, and
//! answers with [`media::cpu_convert_command`], `GetMedia`'s command.
//! `GetMediaDirs` (revision 19) names a kind's user directories, the
//! videos root among them ([`media_dirs`]); `GetMedia`'s reply is a
//! released signature and stays as it is.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use obc_ipc::settings::{VariantDict, WireChange, camera_option};
use obc_ipc::{
    CameraControl, CameraControls, CameraInfo, INTERFACE, IpcError, MediaDirs, MediaEntry,
    MediaReply, OBJECT_PATH, SettingValue, Snapshot, State,
};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use zbus::fdo::NameOwnerChangedStream;
use zbus::message::Header;
use zbus::names::{BusName, InterfaceName};
use zbus::object_server::SignalEmitter;
use zbus::proxy::CacheProperties;
use zbus::{Connection, OwnedVariant, interface};

use crate::controller::{
    Change, Command, ConfigFilesReply, ConfigSchemaReply, ControlsReply, MovesReply, Note,
    PathReply, Published, Reply, SettingsReply,
};
use crate::media::{self, MediaError, MediaItem, MediaKind, MediaStore, MediaType, PosterMaker};

/// The D-Bus object (see the module docs).
pub struct Server {
    published: Published,
    commands: Sender<Command>,
    /// The media gallery, or why there is none (its message).
    media: Result<Arc<MediaStore>, String>,
    /// `MediaSerial`.
    media_serial: Arc<AtomicU32>,
    /// The maker of the posters `GetMedia` found missing (with a gallery).
    posters: Option<PosterMaker>,
    /// Each poster made, until the first `GetMedia` takes it to raise
    /// `MediaSerial` ([`Server::follow_posters`]).
    posters_made: Mutex<Option<UnboundedReceiver<()>>>,
}

impl Server {
    /// The object over the controller's snapshot and command channel, and
    /// the media gallery `media` (normally [`MediaStore::from_env`]; its
    /// error is the reason the gallery methods answer `Failed`).
    pub fn new(
        published: Published,
        commands: Sender<Command>,
        media: Result<MediaStore, MediaError>,
    ) -> Self {
        let (made_tx, made_rx) = unbounded_channel();
        let posters = media.as_ref().ok().map(|store| {
            PosterMaker::new(store.clone(), move |_| {
                let _ = made_tx.send(());
            })
        });
        Server {
            published,
            commands,
            media: media.map(Arc::new).map_err(|e| e.to_string()),
            media_serial: Arc::new(AtomicU32::new(1)),
            posters,
            posters_made: Mutex::new(Some(made_rx)),
        }
    }

    /// From the first call on, a task raising `MediaSerial` (and announcing
    /// it through `emitter`) for the posters made: those landing together
    /// raise it once.
    fn follow_posters(&self, emitter: &SignalEmitter<'_>) {
        let Some(mut made) = self
            .posters_made
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        else {
            return;
        };
        let emitter = emitter.to_owned();
        let serial = Arc::clone(&self.media_serial);
        tokio::spawn(async move {
            while made.recv().await.is_some() {
                while made.try_recv().is_ok() {}
                let now = serial.fetch_add(1, Ordering::SeqCst) + 1;
                if let Err(e) = emit_changed(&emitter, "MediaSerial", &now).await {
                    crate::debug!("emitting a signal failed: {e}");
                }
            }
        });
    }

    /// The media gallery, or `Failed` with why there is none.
    fn store(&self) -> Result<Arc<MediaStore>, IpcError> {
        self.media.clone().map_err(|reason| {
            IpcError::Failed(format!("the media gallery is unavailable: {reason}"))
        })
    }

    /// Raises `MediaSerial` and announces it.
    async fn media_changed(&self, emitter: &SignalEmitter<'_>) {
        self.media_serial.fetch_add(1, Ordering::SeqCst);
        if let Err(e) = self.media_serial_changed(emitter).await {
            crate::debug!("emitting a signal failed: {e}");
        }
    }

    fn snap(&self) -> Snapshot {
        self.published.snapshot()
    }

    async fn call(&self, make: impl FnOnce(Reply) -> Command) -> Result<(), IpcError> {
        self.ask(make).await
    }

    /// Sends the command `make` builds and waits for its reply.
    async fn ask<T>(
        &self,
        make: impl FnOnce(tokio::sync::oneshot::Sender<Result<T, IpcError>>) -> Command,
    ) -> Result<T, IpcError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.commands
            .send(make(tx))
            .map_err(|_| IpcError::Failed("the daemon is shutting down".into()))?;
        rx.await
            .unwrap_or_else(|_| Err(IpcError::Failed("the daemon is shutting down".into())))
    }
}

/// The caller's unique bus name, which a preview lease is keyed by.
fn caller(header: &Header<'_>) -> Result<String, IpcError> {
    header.sender().map(|s| s.to_string()).ok_or_else(|| {
        IpcError::Failed(
            "the call has no sender: a preview lease needs a bus connection, which names its \
             holder"
                .into(),
        )
    })
}

/// Whether `name` still has an owner on the bus of `conn`; `true` when
/// the bus cannot tell (the lease then ends with the name's
/// `NameOwnerChanged`).
async fn still_there(conn: &Connection, name: &str) -> bool {
    let Ok(bus_name) = BusName::try_from(name) else {
        return false;
    };
    let dbus = match zbus::fdo::DBusProxy::builder(conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
    {
        Ok(d) => d,
        Err(_) => return true,
    };
    dbus.name_has_owner(bus_name).await.unwrap_or(true)
}

/// The D-Bus error of a gallery operation ([`MediaError::to_ipc`]).
fn media_error(e: MediaError) -> IpcError {
    e.to_ipc()
}

/// Runs the gallery work `f` on the blocking pool, off the executor.
async fn off_executor<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, IpcError> + Send + 'static,
) -> Result<T, IpcError> {
    tokio::task::spawn_blocking(f).await.unwrap_or_else(|e| {
        Err(IpcError::Failed(format!(
            "the media gallery task failed: {e}"
        )))
    })
}

/// `path` as D-Bus text: `None` when it is not UTF-8.
fn path_text(path: &Path) -> Option<String> {
    path.to_str().map(str::to_string)
}

/// The item on the wire; `None` for a path that is not UTF-8, which D-Bus
/// cannot carry (a poster that is not is left out). A video that is not
/// CPU-ready comes with the command that converts it into `convert_dir`
/// (the videos root, [`MediaStore::convert_dir`]).
fn media_entry(item: &MediaItem, convert_dir: &Path) -> Option<MediaEntry> {
    let video = item.media_type == MediaType::Video;
    let convert_command = if video && !item.cpu_ready {
        media::cpu_convert_command(convert_dir, &item.path).unwrap_or_default()
    } else {
        String::new()
    };
    Some(MediaEntry {
        path: path_text(&item.path)?,
        label: item.label.clone(),
        bundled: item.bundled,
        media_type: item.media_type.as_str().into(),
        width: item.width,
        height: item.height,
        duration_ms: item.duration_ms,
        poster: item
            .poster
            .as_deref()
            .and_then(path_text)
            .unwrap_or_default(),
        cpu_ready: video && item.cpu_ready,
        convert_command,
    })
}

/// `GetConvertCommand`'s answer for the video at `path`: the command that
/// converts it into the user's videos folder
/// ([`media::cpu_convert_command`], the very command `GetMedia` gives).
/// Refused with `InvalidArgument` and the reason for a relative path, a
/// file that is not a video or that the video check refuses, a video that
/// already plays without the NVIDIA GPU, and when a path is not UTF-8.
/// Opens and reads the file: never on the executor.
fn convert_command_of(store: &MediaStore, path: &Path) -> Result<String, IpcError> {
    if !path.is_absolute() {
        return Err(media_error(MediaError::NotAbsolute(path.to_path_buf())));
    }
    let shown = path.display();
    gst_obc::video_background::check_video_file(path)
        .map_err(|e| IpcError::InvalidArgument(format!("the video file {shown}: {e}")))?;
    let info = media::check_video(path).map_err(IpcError::InvalidArgument)?;
    if media::is_cpu_ready(&info) {
        return Err(IpcError::InvalidArgument(format!(
            "the video file {shown} already plays without an NVIDIA GPU: there is nothing to \
             convert"
        )));
    }
    let dir = store.convert_dir();
    media::cpu_convert_command(dir, path).ok_or_else(|| {
        IpcError::InvalidArgument(format!(
            "the conversion of {shown} cannot be given as a command: the videos folder {} is \
             not UTF-8",
            dir.display()
        ))
    })
}

/// The `GetMediaDirs` reply for `kind`: its user directory, as `GetMedia`
/// gives it, and its videos directory ([`MediaStore::video_dir`]); `""`
/// for a kind without one, and when it is the user directory itself (the
/// same path, a link or a bind mount: [`MediaStore::user_dirs`]), so a
/// client never lists one folder twice.
fn media_dirs(store: &MediaStore, kind: MediaKind) -> MediaDirs {
    MediaDirs {
        user_dir: store.user_dir(kind).to_string_lossy().into_owned(),
        video_dir: store
            .user_dirs(kind)
            .get(1)
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

/// The `GetMedia` reply for `kind`, at `serial`, and the videos listed
/// without a poster.
fn media_reply(store: &MediaStore, kind: MediaKind, serial: u32) -> (MediaReply, Vec<PathBuf>) {
    let items = store.list(kind);
    let missing = items
        .iter()
        .filter(|i| i.media_type == MediaType::Video && i.poster.is_none())
        .map(|i| i.path.clone())
        .collect();
    let convert_dir = store.convert_dir();
    let reply = MediaReply {
        serial,
        bundled_dir: store.bundled_dir(kind).to_string_lossy().into_owned(),
        user_dir: store.user_dir(kind).to_string_lossy().into_owned(),
        items: items
            .iter()
            .filter_map(|i| media_entry(i, convert_dir))
            .collect(),
    };
    (reply, missing)
}

#[interface(name = "br.com.dias.OmarchyBroadcast1")]
impl Server {
    /// Enables (`true`) or disables (`false`, the privacy state `off`) the
    /// camera. Persists across restarts.
    async fn set_enabled(&self, enabled: bool) -> Result<(), IpcError> {
        if !enabled {
            // Before queueing: a session start in progress then opens no
            // camera.
            self.published.request_disable();
        }
        self.call(|r| Command::SetEnabled(enabled, r)).await
    }

    /// Selects a camera by id, card-name substring, `vid:pid` or
    /// `/dev/videoN`. Persists across restarts.
    async fn select_camera(&self, selector: String) -> Result<(), IpcError> {
        self.call(|r| Command::SelectCamera(selector, r)).await
    }

    /// Selects the camera after `ActiveCamera`, wrapping around.
    async fn next_camera(&self) -> Result<(), IpcError> {
        self.call(Command::NextCamera).await
    }

    /// Re-reads the configuration files, `config.toml` then `config.d/`
    /// (same as SIGHUP).
    async fn reload_config(&self) -> Result<(), IpcError> {
        self.call(|r| Command::ReloadConfig(Some(r))).await
    }

    /// Selects the effect: `none`, `blur` or `replace` (which needs a
    /// background image). Applies to a running stream at once; persists
    /// across restarts.
    async fn set_effect(&self, effect: String) -> Result<(), IpcError> {
        self.call(|r| Command::SetEffect(effect, r)).await
    }

    /// Sets the blur strength, 0.0 to 1.0. Persists across restarts.
    async fn set_blur_strength(&self, strength: f64) -> Result<(), IpcError> {
        self.call(|r| Command::SetBlurStrength(strength, r)).await
    }

    /// Sets the background image of `replace`: an absolute path to a
    /// readable regular file of at most 32 MiB, or from revision 18 a video
    /// the video check accepts (MP4, M4V, MOV, MKV or WebM, at most
    /// 512 MiB), played in a loop behind the person; empty clears it.
    /// Persists across restarts.
    async fn set_background_image(&self, path: String) -> Result<(), IpcError> {
        let p = PathBuf::from(&path);
        // A video is checked here, off the controller's thread.
        let info = if p.is_absolute() {
            off_executor(move || {
                if crate::controller::is_video_file(&p) {
                    media::check_video(&p)
                        .map(Some)
                        .map_err(IpcError::InvalidArgument)
                } else {
                    Ok(None)
                }
            })
            .await?
        } else {
            None
        };
        self.call(|r| Command::SetBackgroundImage(path, info, r))
            .await
    }

    /// Switches video noise removal on or off: it runs before the effect,
    /// or alone with `none`. Applies to a running stream at once; persists
    /// across restarts (revision 4).
    async fn set_denoise(&self, denoise: bool) -> Result<(), IpcError> {
        self.call(|r| Command::SetDenoise(denoise, r)).await
    }

    /// Takes (or renews) the caller's preview lease at `fps`, 15 or 30;
    /// returns the frame file's absolute path. Works in every state
    /// (revision 5).
    async fn acquire_preview(
        &self,
        fps: u32,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<String, IpcError> {
        let name = caller(&header)?;
        let path = self
            .ask(|reply: PathReply| Command::AcquirePreview {
                name: name.clone(),
                fps,
                reply,
            })
            .await?;
        // The caller may have left before the lease was recorded, its
        // NameOwnerChanged handled first: the lease must not outlive it.
        if !still_there(conn, &name).await {
            let _ = self.commands.send(Command::PreviewOwnerGone(name));
        }
        Ok(path)
    }

    /// Ends the caller's preview lease; without one, does nothing
    /// (revision 5).
    async fn release_preview(&self, #[zbus(header)] header: Header<'_>) -> Result<(), IpcError> {
        let name = caller(&header)?;
        self.call(|r| Command::ReleasePreview(name, r)).await
    }

    /// The image controls `camera` exposes (an id of `Cameras`; empty:
    /// `ActiveCamera`), read from the device in any state (revision 6).
    async fn get_camera_controls(&self, camera: String) -> Result<Vec<CameraControl>, IpcError> {
        self.ask(|reply: ControlsReply| Command::GetCameraControls(camera, reply))
            .await
    }

    /// Sets an image control of `camera` and stores it for that camera:
    /// written at once while it streams, otherwise when it next opens.
    /// Persists across restarts (revision 6).
    async fn set_camera_control(
        &self,
        camera: String,
        name: String,
        value: i32,
    ) -> Result<(), IpcError> {
        self.call(|reply| Command::SetCameraControl {
            camera,
            name,
            value,
            reply,
        })
        .await
    }

    /// Forgets the image controls stored for `camera` and puts each back to
    /// the camera's default: at once while it streams, otherwise when it
    /// next opens. Persists across restarts (revision 6).
    async fn reset_camera_controls(&self, camera: String) -> Result<(), IpcError> {
        self.call(|r| Command::ResetCameraControls(camera, r)).await
    }

    /// The settings tree of a camera: `(revision, serial, camera, groups)`.
    /// `options`: `camera` s, an id of `Cameras` (absent or empty:
    /// `ActiveCamera`); other keys are ignored. The camera's groups come
    /// first, then the global ones (the video effects, revision 8). With no
    /// camera present the reply is `(revision, serial, "", <the global
    /// groups>)` (no group at all before revision 8).
    #[zbus(out_args("revision", "serial", "camera", "groups"))]
    async fn get_settings(
        &self,
        options: VariantDict,
    ) -> Result<(u32, u32, String, Vec<VariantDict>), IpcError> {
        let camera = camera_option(&options)?;
        let tree = self
            .ask(|reply: SettingsReply| Command::GetSettings(camera, reply))
            .await?;
        Ok(tree.to_wire())
    }

    /// Sets a camera setting (`options` as for `GetSettings`): written at
    /// once while the camera streams, otherwise stored and applied when it
    /// next opens. Persists across restarts (revision 7). A global setting
    /// (revision 8) is the effect method it stands for (`SetEffect`,
    /// `SetBlurStrength`, `SetDenoise`), the `camera` option ignored.
    async fn set_setting(
        &self,
        id: String,
        value: OwnedVariant,
        options: VariantDict,
    ) -> Result<(), IpcError> {
        let camera = camera_option(&options)?;
        let value = SettingValue::from_variant(&value);
        self.call(|reply| Command::SetSetting {
            camera,
            id,
            value,
            reply,
        })
        .await
    }

    /// Forgets the stored settings in `scope` (`camera`, `group:<id>` or
    /// `setting:<id>`) and puts them back to the camera's defaults: at once
    /// while it streams, otherwise when it next opens (revision 7). A
    /// global group or setting (revision 8) goes back to the
    /// configuration's value, at once; `camera` never touches those.
    async fn reset_settings(&self, scope: String, options: VariantDict) -> Result<(), IpcError> {
        let camera = camera_option(&options)?;
        self.call(|reply| Command::ResetSettings {
            camera,
            scope,
            reply,
        })
        .await
    }

    /// The modules installed (revision 9): `(serial, modules)`, `serial`
    /// being `ModulesSerial` when the list was made, each module a
    /// dictionary (`obc_ipc::modules`).
    #[zbus(out_args("serial", "modules"))]
    async fn get_modules(&self) -> Result<(u32, Vec<VariantDict>), IpcError> {
        let list = self.ask(Command::GetModules).await?;
        Ok(list.to_wire())
    }

    /// Enables or disables module `id` (revision 9): kept across restarts;
    /// it takes effect when the camera next starts (an enabled module's
    /// settings appear at once). Re-enabling a module the daemon disabled
    /// after it crashed it forgives it.
    async fn set_module_enabled(&self, id: String, enabled: bool) -> Result<(), IpcError> {
        self.call(|reply| Command::SetModuleEnabled(id, enabled, reply))
            .await
    }

    /// The stage order in force: every stage the enabled modules offer
    /// (revision 9).
    async fn get_stage_order(&self) -> Result<Vec<String>, IpcError> {
        self.ask(Command::GetStageOrder).await
    }

    /// Sets the stage order (revision 9): every stage the enabled modules
    /// offer, once each, within their slots and requirements; an empty list
    /// is the default order. An order that is not valid is refused with
    /// `InvalidArgument` and the reason. It takes effect when the camera
    /// next starts.
    async fn set_stage_order(&self, order: Vec<String>) -> Result<(), IpcError> {
        self.call(|reply| Command::SetStageOrder(order, reply))
            .await
    }

    /// Sets the backend profile, `quality` or `battery` (revision 9): which
    /// module provides a stage several offer, from the next camera start.
    async fn set_backend_profile(&self, profile: String) -> Result<(), IpcError> {
        self.call(|reply| Command::SetBackendProfile(profile, reply))
            .await
    }

    /// Pins the module that provides `stage` (`""` clears the pin),
    /// whatever the profile says, while it can run it (revision 9), from
    /// the next camera start.
    async fn set_stage_pin(&self, stage: String, module: String) -> Result<(), IpcError> {
        self.call(|reply| Command::SetStagePin(stage, module, reply))
            .await
    }

    /// The stage order in force with the moves the daemon accepts
    /// (revision 10): each stage as `(id, slot, up, down)`, `up` / `down`
    /// whether `SetStageOrder` would accept the order with the stage
    /// swapped with the one before / after it.
    async fn get_stage_order_moves(&self) -> Result<Vec<(String, String, bool, bool)>, IpcError> {
        let moves = self
            .ask(|reply: MovesReply| Command::GetStageOrderMoves(reply))
            .await?;
        Ok(moves
            .into_iter()
            .map(|m| (m.id, m.slot, m.up, m.down))
            .collect())
    }

    /// The config schema (revision 10): `(serial, groups)`, `serial` being
    /// `ConfigSerial` when it was made, each group a dictionary
    /// (`obc_ipc::config`).
    #[zbus(out_args("serial", "groups"))]
    async fn get_config_schema(&self) -> Result<(u32, Vec<VariantDict>), IpcError> {
        let schema = self
            .ask(|reply: ConfigSchemaReply| Command::GetConfigSchema(reply))
            .await?;
        Ok(schema.to_wire())
    }

    /// Sets the configuration key `key` (revision 10): checked as a load
    /// would check the result, written to `config.d/50-panel.toml`, then
    /// applied as a reload would apply it. Refused with `UnknownSetting`
    /// (no such key), `Disabled` (read only, or "Overridden by
    /// config.d/<file>"), `InvalidArgument` (the value), `InvalidConfig`
    /// (another file does not load) or `Failed` (the managed file cannot
    /// be read or written).
    async fn set_config(&self, key: String, value: OwnedVariant) -> Result<(), IpcError> {
        let value = obc_ipc::ConfigValue::from_variant(&value);
        self.call(|reply| Command::SetConfig(key, value, reply))
            .await
    }

    /// Removes the key `key` from `config.d/50-panel.toml` (revision 10),
    /// so that an earlier file's value (or the default) is in force; the
    /// refusals of `SetConfig`.
    async fn reset_config(&self, key: String) -> Result<(), IpcError> {
        self.call(|reply| Command::ResetConfig(key, reply)).await
    }

    /// The configuration files in load order as they are now (revision
    /// 10): `(serial, dir, files)`, each file `(path, status, managed,
    /// reason)`, `path` relative to `dir`, status `loaded`, `absent` or
    /// `error`.
    #[zbus(out_args("serial", "dir", "files"))]
    async fn get_config_files(
        &self,
    ) -> Result<(u32, String, Vec<(String, String, bool, String)>), IpcError> {
        let files = self
            .ask(|reply: ConfigFilesReply| Command::GetConfigFiles(reply))
            .await?;
        Ok(files.to_wire())
    }

    /// Turns the microphone on (the mic host unit enabled and started) or
    /// off (the previous default microphone restored if the Omarchy
    /// Broadcast Mic is the default, then the unit stopped and disabled)
    /// (revision 13). Refused with `Unavailable` while an ADR 0014
    /// filter-chain unit runs (naming it) or without a service manager.
    async fn set_mic_host(&self, on: bool) -> Result<(), IpcError> {
        self.call(|reply| Command::SetMicHost(on, reply)).await
    }

    /// The media gallery of `kind`, `background` or `away` (revision 15):
    /// `(serial, bundled_dir, user_dir, items)`, `serial` being
    /// `MediaSerial` when the list was made, the directories the kind's in
    /// the packaged and the user's gallery, each item a dictionary
    /// (`obc_ipc::media`), the bundled ones first. Refused with
    /// `InvalidArgument` for another kind.
    #[zbus(out_args("serial", "bundled_dir", "user_dir", "items"))]
    async fn get_media(
        &self,
        kind: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(u32, String, String, Vec<VariantDict>), IpcError> {
        let kind: MediaKind = kind.parse().map_err(media_error)?;
        let store = self.store()?;
        // Read before the list: a change made meanwhile is announced after.
        let serial = self.media_serial.load(Ordering::SeqCst);
        let (reply, missing) = off_executor(move || Ok(media_reply(&store, kind, serial))).await?;
        if let Some(posters) = &self.posters
            && !missing.is_empty()
        {
            self.follow_posters(&emitter);
            posters.request(missing);
        }
        Ok(reply.to_wire())
    }

    /// The user directories of `kind`, `background` or `away` (revision
    /// 19): `user_dir` (s), as `GetMedia` gives it, and `video_dir` (s),
    /// where the kind's videos are stored (the videos root for
    /// `background`, `""` for `away`). Refused as `GetMedia` is:
    /// `InvalidArgument` for another kind, `Failed` without a gallery.
    async fn get_media_dirs(&self, kind: String) -> Result<VariantDict, IpcError> {
        let kind: MediaKind = kind.parse().map_err(media_error)?;
        let store = self.store()?;
        Ok(media_dirs(&store, kind).to_dict())
    }

    /// Copies the image at `path` (absolute) into the user's gallery of
    /// `kind` after the checks of a background, and returns the stored
    /// path; a file of that gallery with the same bytes is returned
    /// instead, without a copy (revision 15). A background may also be a
    /// video, stored as it is after its check, or an animated image,
    /// transcoded to a VP9 WebM with ffmpeg ([`MediaStore::import`]). `MediaSerial` moves only when
    /// a file was stored. Refused with `InvalidArgument` and the reason;
    /// `Failed` when the gallery cannot be written.
    async fn import_media(
        &self,
        kind: String,
        path: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<String, IpcError> {
        let kind: MediaKind = kind.parse().map_err(media_error)?;
        let store = self.store()?;
        let src = PathBuf::from(path);
        let (stored, new) =
            off_executor(move || store.import_stored(kind, &src).map_err(media_error)).await?;
        if new {
            self.media_changed(&emitter).await;
        }
        path_text(&stored).ok_or_else(|| {
            IpcError::Failed(format!(
                "{} was stored, but its path is not UTF-8, which D-Bus cannot carry",
                stored.display()
            ))
        })
    }

    /// The shell command that converts the video at `path` (absolute)
    /// into a CPU-ready VP9 WebM in the user's gallery of backgrounds,
    /// `<stem>-cpu.webm` (or the first free `<stem>-cpu-N.webm`): the
    /// command `GetMedia` gives for a gallery video, for any file
    /// (revision 19). The daemon checks the file, as `SetBackgroundImage`
    /// does, and never runs the command. Refused with `InvalidArgument`
    /// and the reason for a relative path, a file that is not a video or
    /// that the check refuses, a video that already plays without the
    /// NVIDIA GPU, and a gallery path that is not UTF-8; `Failed` without
    /// a gallery.
    async fn get_convert_command(&self, path: String) -> Result<String, IpcError> {
        let store = self.store()?;
        off_executor(move || convert_command_of(&store, Path::new(&path))).await
    }

    /// Removes the item at `path` from the user's gallery (revision 15).
    /// Refused with `InvalidArgument` for a bundled item or any path that
    /// is not a user item, and while the configuration names the file
    /// (`[effect] background_image` or `fallback_image`). When it is the
    /// run-time background, that choice is cleared first, as an empty
    /// `SetBackgroundImage` does, and a refusal of that refuses the
    /// removal with its error. The check, the clear and the removal are
    /// one controller step ([`Command::ReleaseMedia`]), so no
    /// `SetBackgroundImage` can choose the file in between. `Failed` when
    /// the file cannot be removed (the cleared choice is restored).
    async fn remove_media(
        &self,
        path: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), IpcError> {
        let store = self.store()?;
        let target = PathBuf::from(&path);
        {
            let (store, target) = (Arc::clone(&store), target.clone());
            off_executor(move || {
                if !target.is_absolute() {
                    return Err(media_error(MediaError::NotAbsolute(target)));
                }
                if store
                    .is_gallery_item(&target)
                    .is_none_or(|(_, bundled)| bundled)
                {
                    return Err(media_error(MediaError::NotAUserItem(target)));
                }
                // Here, not on the controller: a video's content hash,
                // which names its poster.
                store.warm_hash(&target);
                Ok(())
            })
            .await?;
        }
        self.call(|reply| Command::ReleaseMedia {
            store,
            path: target,
            reply,
        })
        .await?;
        self.media_changed(&emitter).await;
        Ok(())
    }

    /// About once per second while `MicState` is `active`, relayed from the
    /// audio worker (revision 13).
    #[zbus(signal)]
    pub async fn mic_stats(
        emitter: &SignalEmitter<'_>,
        overruns: u32,
        misses: u32,
        voice: f64,
    ) -> zbus::Result<()>;

    /// The configuration in force, its files or the config schema changed;
    /// `serial` is the new `ConfigSerial` (revision 10).
    #[zbus(signal)]
    pub async fn config_changed(emitter: &SignalEmitter<'_>, serial: u32) -> zbus::Result<()>;

    /// The settings tree changed: one signal per operation, each setting
    /// whose value, `stored`, `enabled` or reason changed; empty when
    /// `serial` moved (the client calls `GetSettings`) (revision 7).
    #[zbus(signal)]
    pub async fn settings_changed(
        emitter: &SignalEmitter<'_>,
        revision: u32,
        serial: u32,
        changes: Vec<WireChange>,
    ) -> zbus::Result<()>;

    /// After each replace of the preview frame file while a lease is held;
    /// `seq` increases strictly within a daemon run, from 1 (revision 5).
    #[zbus(signal)]
    pub async fn preview_frame(emitter: &SignalEmitter<'_>, seq: u32) -> zbus::Result<()>;

    /// About once per second while `active`; `-1` marks a value that cannot
    /// be measured.
    #[zbus(signal)]
    pub async fn stats(
        emitter: &SignalEmitter<'_>,
        fps: f64,
        latency_p50_ms: f64,
        cpu_percent: f64,
        gpu_percent: f64,
    ) -> zbus::Result<()>;

    #[zbus(property)]
    fn state(&self) -> State {
        self.snap().state
    }

    #[zbus(property)]
    fn error(&self) -> String {
        self.snap().error
    }

    /// What runs other than the user asked, without being an error: the
    /// selected stages no module provides, dropped while the feed streams
    /// on without them, and an effect a module provides that cannot run
    /// (the feed black with `on_failure = "black"`, the camera image with
    /// `"none"`); one line, empty when none (revision 11).
    #[zbus(property)]
    fn warning(&self) -> String {
        self.snap().warning
    }

    #[zbus(property)]
    fn enabled(&self) -> bool {
        self.snap().enabled
    }

    #[zbus(property)]
    fn cameras(&self) -> Vec<CameraInfo> {
        self.snap().cameras
    }

    #[zbus(property)]
    fn active_camera(&self) -> String {
        self.snap().active_camera
    }

    #[zbus(property)]
    fn output_mode(&self) -> String {
        self.snap().output_mode
    }

    #[zbus(property)]
    fn effect(&self) -> String {
        self.snap().effect
    }

    #[zbus(property)]
    fn blur_strength(&self) -> f64 {
        self.snap().blur_strength
    }

    #[zbus(property)]
    fn background_image(&self) -> String {
        self.snap().background_image
    }

    /// Whether video noise removal is requested (revision 4).
    #[zbus(property)]
    fn denoise(&self) -> bool {
        self.snap().denoise
    }

    /// Every consumer: the processes holding the loopback, the links to the
    /// PipeWire output and the preview leases.
    #[zbus(property)]
    fn consumers(&self) -> u32 {
        self.snap().consumers
    }

    /// The processes holding the loopback device open (revision 3).
    #[zbus(property)]
    fn loopback_consumers(&self) -> u32 {
        self.snap().loopback_consumers
    }

    /// The links to the PipeWire output; 0 when it is off (revision 3).
    #[zbus(property, name = "PipeWireConsumers")]
    fn pipewire_consumers(&self) -> u32 {
        self.snap().pipewire_consumers
    }

    /// The preview leases held (revision 5).
    #[zbus(property)]
    fn preview_consumers(&self) -> u32 {
        self.snap().preview_consumers
    }

    /// The PipeWire output's state: `off`, `unsupported`, `connecting`,
    /// `ready`, `streaming` or `failed` (revision 3).
    #[zbus(property, name = "PipeWireOutput")]
    fn pipewire_output(&self) -> String {
        self.snap().pipewire_output
    }

    /// The image controls of `ActiveCamera` (camera `""` and none without
    /// a camera) (revision 6).
    #[zbus(property)]
    fn camera_controls(&self) -> CameraControls {
        self.snap().camera_controls.unwrap_or_default()
    }

    /// Bumped at every change of the published settings tree; from 1
    /// (revision 7).
    #[zbus(property)]
    fn settings_revision(&self) -> u32 {
        self.snap().settings_revision.unwrap_or(1)
    }

    /// Bumped at a structural change of the settings tree only; from 1
    /// (revision 7).
    #[zbus(property)]
    fn settings_schema_serial(&self) -> u32 {
        self.snap().settings_schema_serial.unwrap_or(1)
    }

    /// Bumped whenever what `GetModules` returns changes; from 1
    /// (revision 9).
    #[zbus(property)]
    fn modules_serial(&self) -> u32 {
        self.snap().modules_serial.unwrap_or(1)
    }

    /// The backend profile in force, `quality` or `battery` (revision 9).
    #[zbus(property)]
    fn backend_profile(&self) -> String {
        self.snap()
            .backend_profile
            .unwrap_or_else(|| "quality".into())
    }

    /// Bumped whenever what `GetConfigSchema` or `GetConfigFiles` returns
    /// changes through the daemon (a write, a reload, the modules found);
    /// from 1 (revision 10).
    #[zbus(property)]
    fn config_serial(&self) -> u32 {
        self.snap().config_serial.unwrap_or(1)
    }

    /// The microphone: `off` (the mic host does not run), `passthrough`,
    /// `starting`, `active` or `error` (revision 13).
    #[zbus(property)]
    fn mic_state(&self) -> String {
        self.snap().mic.unwrap_or_default().state
    }

    /// The applications recording from the microphone (revision 13).
    #[zbus(property)]
    fn mic_consumers(&self) -> u32 {
        self.snap().mic.unwrap_or_default().consumers
    }

    /// The module running each microphone stage, as text (revision 13).
    #[zbus(property)]
    fn mic_provider(&self) -> String {
        self.snap().mic.unwrap_or_default().provider
    }

    /// The delay the microphone processing adds, in milliseconds
    /// (revision 13).
    #[zbus(property)]
    fn mic_latency_ms(&self) -> f64 {
        self.snap().mic.unwrap_or_default().latency_ms
    }

    /// Why microphone stages that are on cannot run (revision 13).
    #[zbus(property)]
    fn mic_error(&self) -> String {
        self.snap().mic.unwrap_or_default().error
    }

    /// Bumped after each import that stored a file, each removal from the
    /// media gallery and each poster made for a video `GetMedia` listed
    /// without one; from 1, restarting at 1 at each daemon start, so
    /// compared only within one daemon run (revision 15).
    #[zbus(property)]
    fn media_serial(&self) -> u32 {
        self.media_serial.load(Ordering::SeqCst)
    }

    /// The named numeric values the running stages report about the last
    /// frame, `(stage, name, value)`: auto-frame's `zoom`, for one. Empty
    /// when the camera is idle or no stage reports. Announced when the set
    /// changes or a value moves by 0.01 or more, at most every 250 ms
    /// (revision 16).
    #[zbus(property)]
    fn stage_values(&self) -> Vec<obc_ipc::StageValue> {
        self.snap().stage_values
    }

    #[zbus(property)]
    fn interface_revision(&self) -> u32 {
        self.snap().interface_revision
    }
}

/// Emits `PropertiesChanged` for one property with `value`.
async fn emit_changed<T>(emitter: &SignalEmitter<'_>, name: &str, value: &T) -> zbus::Result<()>
where
    T: serde::Serialize + zbus::Type,
{
    let mut changed = HashMap::new();
    changed.insert(name, zbus::as_variant::Serialize(value));
    emitter
        .emit(
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &(
                InterfaceName::from_static_str_unchecked(INTERFACE),
                changed,
                Cow::<[&str]>::Borrowed(&[]),
            ),
        )
        .await
}

async fn emit_change(emitter: &SignalEmitter<'_>, change: &Change) -> zbus::Result<()> {
    let name = change.name();
    match change {
        Change::State(v) => emit_changed(emitter, name, v).await,
        Change::Error(v)
        | Change::Warning(v)
        | Change::ActiveCamera(v)
        | Change::OutputMode(v)
        | Change::Effect(v)
        | Change::BackgroundImage(v)
        | Change::PipeWireOutput(v)
        | Change::BackendProfile(v)
        | Change::MicState(v)
        | Change::MicProvider(v)
        | Change::MicError(v) => emit_changed(emitter, name, v).await,
        Change::BlurStrength(v) | Change::MicLatencyMs(v) => emit_changed(emitter, name, v).await,
        Change::Enabled(v) | Change::Denoise(v) => emit_changed(emitter, name, v).await,
        Change::Cameras(v) => emit_changed(emitter, name, v).await,
        Change::Consumers(v)
        | Change::LoopbackConsumers(v)
        | Change::PipeWireConsumers(v)
        | Change::PreviewConsumers(v)
        | Change::SettingsRevision(v)
        | Change::SettingsSchemaSerial(v)
        | Change::ModulesSerial(v)
        | Change::ConfigSerial(v)
        | Change::MicConsumers(v) => emit_changed(emitter, name, v).await,
        Change::CameraControls(v) => emit_changed(emitter, name, v).await,
        Change::StageValues(v) => emit_changed(emitter, name, v).await,
    }
}

/// Emits the controller's notes on `conn` until the controller is gone.
pub async fn run_publisher(conn: Connection, mut notes: UnboundedReceiver<Note>) {
    let emitter = match SignalEmitter::new(&conn, OBJECT_PATH) {
        Ok(e) => e,
        Err(e) => {
            crate::error!("D-Bus signals unavailable: {e}");
            return;
        }
    };
    while let Some(note) = notes.recv().await {
        let res = match &note {
            Note::Changed(changes) => {
                let mut res = Ok(());
                for c in changes {
                    res = res.and(emit_change(&emitter, c).await);
                }
                res
            }
            Note::Stats(s) => {
                Server::stats(
                    &emitter,
                    s.fps,
                    s.latency_p50_ms,
                    s.cpu_percent,
                    s.gpu_percent,
                )
                .await
            }
            Note::PreviewFrame(seq) => Server::preview_frame(&emitter, *seq).await,
            Note::SettingsChanged {
                revision,
                serial,
                changes,
            } => {
                let changes: Vec<WireChange> = changes.iter().map(|c| c.to_wire()).collect();
                Server::settings_changed(&emitter, *revision, *serial, changes).await
            }
            Note::ConfigChanged(serial) => Server::config_changed(&emitter, *serial).await,
            Note::MicStats(overruns, misses, voice) => {
                Server::mic_stats(&emitter, *overruns, *misses, *voice).await
            }
        };
        if let Err(e) = res {
            crate::debug!("emitting a signal failed: {e}");
        }
    }
}

/// Subscribes to `NameOwnerChanged` on the bus of `conn`, for
/// [`run_owner_watch`]. Done before `AcquirePreview` is served, so that no
/// departure after a lease was recorded is missed.
pub async fn subscribe_owners(conn: &Connection) -> zbus::Result<NameOwnerChangedStream> {
    let dbus = zbus::fdo::DBusProxy::new(conn).await?;
    dbus.receive_name_owner_changed().await
}

/// Tells the controller of every unique name that leaves the bus
/// (`NameOwnerChanged` with no new owner, from [`subscribe_owners`]), which
/// ends its preview lease if it holds one, until the connection closes or
/// the controller is gone.
pub async fn run_owner_watch(
    mut owners: NameOwnerChangedStream,
    commands: std::sync::mpsc::Sender<Command>,
) {
    while let Some(signal) = owners.next().await {
        let Ok(args) = signal.args() else { continue };
        if args.new_owner().is_none()
            && let BusName::Unique(name) = args.name()
            && commands
                .send(Command::PreviewOwnerGone(name.to_string()))
                .is_err()
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests;
