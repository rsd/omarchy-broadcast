//! The client's view of the daemon: an [`obc_ipc::Snapshot`] (read and
//! updated by `obc-ipc`), compared to find what changed.

use obc_ipc::{CameraInfo, Snapshot};

/// The JSON names of the scalar fields of a `state` line, in output order
/// (`docs/cli-json.md`).
pub const STATE_FIELDS: [&str; 16] = [
    "state",
    "enabled",
    "error",
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

/// The entry of `ActiveCamera` in `Cameras`, if it is listed.
pub fn active_camera(snap: &Snapshot) -> Option<&CameraInfo> {
    if snap.active_camera.is_empty() {
        return None;
    }
    snap.cameras.iter().find(|c| c.id == snap.active_camera)
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
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty() && !self.cameras
    }
}

/// The changes from `old` to `new`.
pub fn diff(old: &Snapshot, new: &Snapshot) -> Changes {
    let checks = [
        old.state != new.state,
        old.enabled != new.enabled,
        old.error != new.error,
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
                cameras: false
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
                cameras: true
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
}
