//! The client's view of the daemon: an [`obc_ipc::Snapshot`] (read and
//! updated by `obc-ipc`), compared to find what changed.

use obc_ipc::{CameraInfo, Snapshot};

/// The JSON names of the scalar fields of a `state` line, in output order
/// (`docs/cli-json.md`).
pub const STATE_FIELDS: [&str; 11] = [
    "state",
    "enabled",
    "error",
    "active_camera",
    "active_camera_name",
    "consumers",
    "output_mode",
    "effect",
    "blur_strength",
    "background_image",
    "interface_revision",
];

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
        old.output_mode != new.output_mode,
        old.effect != new.effect,
        // Bitwise, so that a NaN does not count as a change every time.
        old.blur_strength.to_bits() != new.blur_strength.to_bits(),
        old.background_image != new.background_image,
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

        let mut nan = old.clone();
        nan.blur_strength = f64::NAN;
        assert!(diff(&nan, &nan.clone()).is_empty());
    }
}
