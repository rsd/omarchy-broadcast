//! The client's view of the daemon: an [`obc_ipc::Snapshot`] (read and
//! updated by `obc-ipc`), compared to find what changed.

use obc_ipc::{CameraInfo, Snapshot};

/// The JSON names of the scalar fields of a `state` line, in output order
/// (`docs/cli-json.md`).
pub const STATE_FIELDS: [&str; 17] = [
    "state",
    "enabled",
    "error",
    "warning",
    "active_camera",
    "active_camera_name",
    "consumers",
    "loopback_consumers",
    "pipewire_consumers",
    "preview_consumers",
    "pipewire_output",
    "output_mode",
    "effect",
    "blur_strength",
    "background_image",
    "denoise",
    "interface_revision",
];

/// Whether the PipeWire output is part of the picture: configured (its
/// state is not `off`) or, briefly after it stopped, still counted.
pub fn pipewire_involved(snap: &Snapshot) -> bool {
    snap.pipewire_output != obc_ipc::PIPEWIRE_OFF || snap.pipewire_consumers > 0
}

/// Whether noise removal is requested; `None` from a daemon too old to have
/// it (before [`obc_ipc::DENOISE_REVISION`]), whose `false` means nothing.
pub fn denoise(snap: &Snapshot) -> Option<bool> {
    (snap.interface_revision >= obc_ipc::DENOISE_REVISION).then_some(snap.denoise)
}

/// Whether the daemon has the camera settings (revision
/// [`obc_ipc::SETTINGS_REVISION`] or later).
pub fn has_settings(snap: &Snapshot) -> bool {
    snap.interface_revision >= obc_ipc::SETTINGS_REVISION
}

/// `SettingsRevision`; `None` from a daemon too old to have it.
pub fn settings_revision(snap: &Snapshot) -> Option<u32> {
    snap.settings_revision.filter(|_| has_settings(snap))
}

/// The entry of `ActiveCamera` in `Cameras`, if it is listed.
pub fn active_camera(snap: &Snapshot) -> Option<&CameraInfo> {
    camera(snap, &snap.active_camera)
}

/// The entry of the camera `id` in `Cameras`, if it is listed.
pub fn camera<'a>(snap: &'a Snapshot, id: &str) -> Option<&'a CameraInfo> {
    if id.is_empty() {
        return None;
    }
    snap.cameras.iter().find(|c| c.id == id)
}

/// The name of the active camera, if it is listed.
pub fn active_camera_name(snap: &Snapshot) -> Option<&str> {
    active_camera(snap).map(|c| c.name.as_str())
}

/// What differs between two snapshots.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Changes {
    /// The [`STATE_FIELDS`] whose value changed, in that order.
    pub fields: Vec<&'static str>,
    /// Whether the camera list changed.
    pub cameras: bool,
    /// Whether `CameraControls` changed (revision 6): its camera, or a
    /// control's range, default, value or stored mark.
    pub controls: bool,
    /// Whether one of the microphone's properties changed (revision 13).
    pub mic: bool,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty() && !self.cameras && !self.controls && !self.mic
    }
}

/// The changes from `old` to `new`.
pub fn diff(old: &Snapshot, new: &Snapshot) -> Changes {
    let checks = [
        old.state != new.state,
        old.enabled != new.enabled,
        old.error != new.error,
        old.warning != new.warning,
        old.active_camera != new.active_camera,
        active_camera_name(old) != active_camera_name(new),
        old.consumers != new.consumers,
        old.loopback_consumers != new.loopback_consumers,
        old.pipewire_consumers != new.pipewire_consumers,
        old.preview_consumers != new.preview_consumers,
        old.pipewire_output != new.pipewire_output,
        old.output_mode != new.output_mode,
        old.effect != new.effect,
        // Bitwise, so that a NaN does not count as a change every time.
        old.blur_strength.to_bits() != new.blur_strength.to_bits(),
        old.background_image != new.background_image,
        // As printed: from unknown to a value is a change.
        denoise(old) != denoise(new),
        old.interface_revision != new.interface_revision,
    ];
    Changes {
        fields: STATE_FIELDS
            .iter()
            .zip(checks)
            .filter(|(_, changed)| *changed)
            .map(|(name, _)| *name)
            .collect(),
        cameras: old.cameras != new.cameras,
        controls: old.camera_controls != new.camera_controls,
        mic: old.mic != new.mic,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use obc_ipc::State;

    fn cam(id: &str, name: &str) -> CameraInfo {
        CameraInfo {
            id: id.into(),
            name: name.into(),
            usb: String::new(),
        }
    }

    #[test]
    fn diff_names_changed_fields_in_order() {
        let mut old = Snapshot::empty();
        old.cameras = vec![cam("a", "A"), cam("b", "B")];
        old.active_camera = "a".into();
        assert!(diff(&old, &old).is_empty());

        let mut new = old.clone();
        new.consumers = 1;
        new.state = State::Active;
        assert_eq!(
            diff(&old, &new),
            Changes {
                fields: vec!["state", "consumers"],
                cameras: false,
                controls: false,
                mic: false
            }
        );

        let mut new = old.clone();
        new.active_camera = "b".into();
        assert_eq!(
            diff(&old, &new).fields,
            vec!["active_camera", "active_camera_name"]
        );

        // A renamed active camera changes the derived name and the list.
        let mut new = old.clone();
        new.cameras[0].name = "A2".into();
        new.effect = "blur".into();
        new.blur_strength = 0.8;
        new.background_image = "/bg.png".into();
        assert_eq!(
            diff(&old, &new),
            Changes {
                fields: vec![
                    "active_camera_name",
                    "effect",
                    "blur_strength",
                    "background_image"
                ],
                cameras: true,
                controls: false,
                mic: false
            }
        );
        assert_eq!(active_camera_name(&new), Some("A2"));
        new.active_camera = "gone".into();
        assert_eq!(active_camera_name(&new), None);

        // The PipeWire fields.
        let mut new = old.clone();
        new.consumers = 2;
        new.loopback_consumers = 1;
        new.pipewire_consumers = 1;
        new.pipewire_output = "streaming".into();
        assert_eq!(
            diff(&old, &new).fields,
            vec![
                "consumers",
                "loopback_consumers",
                "pipewire_consumers",
                "pipewire_output"
            ]
        );
        assert!(!pipewire_involved(&old));
        assert!(pipewire_involved(&new));
        new.pipewire_output = "off".into();
        assert!(pipewire_involved(&new), "still counted");
        new.pipewire_consumers = 0;
        assert!(!pipewire_involved(&new));

        let mut nan = old.clone();
        nan.blur_strength = f64::NAN;
        assert!(diff(&nan, &nan.clone()).is_empty());

        // Denoise: unknown before revision 4, so an older daemon's `false`
        // is no value, and a newer daemon's arrival is a change of it.
        let mut rev3 = old.clone();
        rev3.interface_revision = 3;
        assert_eq!(denoise(&rev3), None);
        let mut rev4 = rev3.clone();
        rev4.interface_revision = obc_ipc::DENOISE_REVISION;
        assert_eq!(denoise(&rev4), Some(false));
        assert_eq!(
            diff(&rev3, &rev4).fields,
            vec!["denoise", "interface_revision"]
        );
        let mut on = rev4.clone();
        on.denoise = true;
        assert_eq!(denoise(&on), Some(true));
        assert_eq!(diff(&rev4, &on).fields, vec!["denoise"]);
    }

    #[test]
    fn diff_tells_a_change_of_the_camera_controls() {
        use obc_ipc::{CameraControl, CameraControls};
        let mut old = Snapshot::empty();
        old.camera_controls = Some(CameraControls {
            camera: "a".into(),
            controls: vec![CameraControl {
                name: "sharpness".into(),
                min: 0,
                max: 255,
                step: 1,
                default: 128,
                value: 128,
                stored: false,
            }],
        });
        assert!(diff(&old, &old.clone()).is_empty());
        let mut new = old.clone();
        new.camera_controls.as_mut().unwrap().controls[0].stored = true;
        let changes = diff(&old, &new);
        assert_eq!(
            changes,
            Changes {
                fields: Vec::new(),
                cameras: false,
                controls: true,
                mic: false
            }
        );
        assert!(!changes.is_empty());
        let mut other = old.clone();
        other.camera_controls.as_mut().unwrap().camera = "b".into();
        assert!(diff(&old, &other).controls);
        // From an older daemon (no property) to a newer one: a change.
        assert!(diff(&Snapshot::empty(), &old).controls);
    }

    #[test]
    fn diff_tells_a_change_of_the_microphone() {
        let mut old = Snapshot::empty();
        old.interface_revision = obc_ipc::MIC_REVISION;
        // From an older daemon (no properties) to a newer one: a change.
        let mut first = old.clone();
        first.mic = Some(obc_ipc::MicStatus::default());
        assert!(diff(&old, &first).mic);
        old = first;
        let mut new = old.clone();
        new.mic.as_mut().unwrap().state = "active".into();
        let changes = diff(&old, &new);
        assert!(changes.mic && changes.fields.is_empty() && !changes.is_empty());
        assert!(!diff(&old, &old.clone()).mic);
    }
}
