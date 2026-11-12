//! The models the module can run: one entry per model, with its file, its
//! checksum, the input it takes and how its output reads as a matte.
//!
//! The pre-processing of each entry is the one the spike measured
//! (`model-sources/README.md`): the frame scaled to the model's input
//! (an area average, ignoring the aspect ratio for the square models, as
//! the spike's `cv2.resize` did), RGB, normalised to `[0, 1]` or `[-1, 1]`.

/// The memory layout of the model's image input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// `[1, 3, H, W]`: one plane per colour.
    Nchw,
    /// `[1, H, W, 3]`: the colours of a pixel together.
    Nhwc,
}

/// How the 8-bit RGB values are normalised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Norm {
    /// `v / 255`, in `[0, 1]`.
    Unit,
    /// `(v / 255 - 0.5) / 0.5`, in `[-1, 1]`.
    Signed,
}

/// How the model's output reads as the person's matte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// One channel: the person's alpha (`[1, H, W, 1]` or `[1, 1, H, W]`).
    Alpha,
    /// Two channels after a softmax, `[1, 2, H, W]`: background, person;
    /// the matte is the second.
    Softmax2,
}

/// The input size of a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Geometry {
    /// A fixed `height x width`.
    Fixed { height: u32, width: u32 },
    /// A dynamic size: the frame scaled so its short side is `short`,
    /// both sides rounded to a multiple of `multiple`.
    ShortSide { short: u32, multiple: u32 },
}

/// One model of the catalogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelSpec {
    /// The value of the `model` parameter (`effect.onnx.model`).
    pub id: &'static str,
    /// The option's label in the settings.
    pub label: &'static str,
    /// The file name in the model directory.
    pub file: &'static str,
    /// The SHA-256 of the file, lower-case hex.
    pub sha256: &'static str,
    /// The image input's layout.
    pub layout: Layout,
    /// The normalisation of the input.
    pub norm: Norm,
    /// The output's meaning.
    pub output: Output,
    /// The input size.
    pub geometry: Geometry,
}

impl ModelSpec {
    /// The model's input size, `(height, width)`, for frames of
    /// `width x height`.
    pub fn input_size(&self, width: u32, height: u32) -> (u32, u32) {
        match self.geometry {
            Geometry::Fixed { height, width } => (height, width),
            Geometry::ShortSide { short, multiple } => {
                let (w, h) = (f64::from(width.max(1)), f64::from(height.max(1)));
                let s = f64::from(short) / w.min(h);
                let m = f64::from(multiple.max(1));
                let round = |v: f64| ((v * s / m).round() * m).max(m) as u32;
                (round(h), round(w))
            }
        }
    }

    /// The input tensor's shape for an input of `(height, width)`.
    pub fn input_shape(&self, (h, w): (u32, u32)) -> [i64; 4] {
        let (h, w) = (i64::from(h), i64::from(w));
        match self.layout {
            Layout::Nchw => [1, 3, h, w],
            Layout::Nhwc => [1, h, w, 3],
        }
    }

    /// The number of output values the model gives for an input of
    /// `(height, width)`.
    pub fn output_len(&self, (h, w): (u32, u32)) -> usize {
        let plane = h as usize * w as usize;
        match self.output {
            Output::Alpha => plane,
            Output::Softmax2 => 2 * plane,
        }
    }
}

/// The default model: MediaPipe's selfie segmentation (landscape).
pub const DEFAULT_MODEL: &str = "mediapipe-landscape";

/// The catalogue, in the order the settings offer it.
///
/// The MediaPipe file is our own conversion of Google's
/// `selfie_segmentation_landscape.tflite` (tf2onnx and `fix_tconv.py`), not
/// the community `mp_selfie_landscape.onnx`: the two give the same matte
/// (a largest difference of 3.8e-5), and ours is reproducible from the
/// upstream file with the scripts in `model-sources/`, so its provenance is
/// one step from Google's release. Ours keeps the TFLite layout, NHWC.
pub const MODELS: [ModelSpec; 4] = [
    ModelSpec {
        id: DEFAULT_MODEL,
        label: "MediaPipe (fast)",
        file: "selfie_segmentation_landscape.onnx",
        sha256: "fa02222275c455638fa1190493b659d25ad31e86f1b6459f17bd501f02cbe43d",
        layout: Layout::Nhwc,
        norm: Norm::Unit,
        output: Output::Alpha,
        geometry: Geometry::Fixed {
            height: 144,
            width: 256,
        },
    },
    ModelSpec {
        id: "pphumanseg-v2-mobile",
        label: "PP-HumanSeg mobile (quality)",
        file: "pp_humansegv2_mobile_192x192.onnx",
        sha256: "2bca11ec1f2a662146147d97523d613f5ef4ebf2a89803a95a0cbabfe2bd70a9",
        layout: Layout::Nchw,
        norm: Norm::Signed,
        output: Output::Softmax2,
        geometry: Geometry::Fixed {
            height: 192,
            width: 192,
        },
    },
    ModelSpec {
        id: "pphumanseg-v2-lite",
        label: "PP-HumanSeg lite",
        file: "pp_humansegv2_lite_192x192.onnx",
        sha256: "ca96b4436f1ce6a76059e8902e20a9617f6829fb11f6c6c9e876563e6352e6b4",
        layout: Layout::Nchw,
        norm: Norm::Signed,
        output: Output::Softmax2,
        geometry: Geometry::Fixed {
            height: 192,
            width: 192,
        },
    },
    ModelSpec {
        id: "modnet",
        label: "MODNet (sharp hair, GPU)",
        file: "modnet_photographic.onnx",
        sha256: "07c308cf0fc7e6e8b2065a12ed7fc07e1de8febb7dc7839d7b7f15dd66584df9",
        layout: Layout::Nchw,
        norm: Norm::Signed,
        output: Output::Alpha,
        geometry: Geometry::ShortSide {
            short: 288,
            multiple: 32,
        },
    },
];

/// The model of id `id`.
pub fn find(id: &str) -> Option<&'static ModelSpec> {
    MODELS.iter().find(|m| m.id == id)
}

/// The default model.
pub fn default_model() -> &'static ModelSpec {
    find(DEFAULT_MODEL).expect("the default model is in the catalogue")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalogue_is_in_the_users_order_with_unique_ids_and_files() {
        let ids: Vec<&str> = MODELS.iter().map(|m| m.id).collect();
        assert_eq!(
            ids,
            [
                "mediapipe-landscape",
                "pphumanseg-v2-mobile",
                "pphumanseg-v2-lite",
                "modnet"
            ]
        );
        assert_eq!(default_model().id, ids[0]);
        for (i, m) in MODELS.iter().enumerate() {
            assert_eq!(m.sha256.len(), 64, "{}", m.id);
            assert!(
                m.sha256
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            );
            assert!(m.label.len() <= 48);
            assert!(m.file.ends_with(".onnx"));
            for n in &MODELS[i + 1..] {
                assert_ne!(m.file, n.file);
                assert_ne!(m.sha256, n.sha256);
            }
        }
        assert!(find("nope").is_none());
    }

    #[test]
    fn input_sizes_and_shapes() {
        let mp = default_model();
        assert_eq!(mp.input_size(1920, 1080), (144, 256));
        assert_eq!(mp.input_shape((144, 256)), [1, 144, 256, 3]);
        assert_eq!(mp.output_len((144, 256)), 144 * 256);
        let pp = find("pphumanseg-v2-mobile").unwrap();
        assert_eq!(pp.input_size(1280, 720), (192, 192));
        assert_eq!(pp.input_shape((192, 192)), [1, 3, 192, 192]);
        assert_eq!(pp.output_len((192, 192)), 2 * 192 * 192);
        // MODNet at a short side of 288, multiples of 32, as the spike ran it.
        let modnet = find("modnet").unwrap();
        assert_eq!(modnet.input_size(1920, 1080), (288, 512));
        assert_eq!(modnet.input_size(1280, 720), (288, 512));
        assert_eq!(modnet.input_size(640, 480), (288, 384));
        assert_eq!(modnet.input_size(720, 1280), (512, 288));
    }
}
