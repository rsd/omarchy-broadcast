//! What the integration tests share: the module's library (built by cargo,
//! in release, into a directory under `CARGO_TARGET_TMPDIR`), its manifest
//! installed beside it, the module opened through `obc-module-host` as the
//! daemon opens it, and host NV12 frames.

#![allow(dead_code)] // each test binary uses part of it

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use obc_backend::{HostBgr, StageId, Value};
use obc_module_host::module::ChainSpec;
use obc_module_host::{LogLevel, LogSink, Manifest, Module};
use obc_module_sdk::abi::{
    OBC_CHROMA_SITING_MPEG2, OBC_FORMAT_NV12, OBC_FRAME_HOST, OBC_MATRIX_BT709, obc_cuda,
    obc_frame, obc_plane,
};
use obc_module_sdk::serde_json;

pub const ID: &str = "br.com.dias.onnx-cutout";
pub const PACKAGED: &str = "library = \"/usr/lib/omarchy-broadcast/modules/libobc_module_onnx.so\"";

pub fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// `libobc_module_onnx.so`, built once per test binary, in release (the
/// timings are measured on it), with the crate's own `Cargo.lock`.
pub fn library() -> PathBuf {
    static LIB: OnceLock<PathBuf> = OnceLock::new();
    LIB.get_or_init(|| {
        let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("onnx-module");
        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "--quiet",
                "--locked",
                "--release",
                "--manifest-path",
            ])
            .arg(crate_dir().join("Cargo.toml"))
            .arg("--target-dir")
            .arg(&target)
            .status()
            .expect("run cargo");
        assert!(status.success(), "building obc-module-onnx failed");
        let path = target.join("release/libobc_module_onnx.so");
        assert!(path.is_file(), "{} was not built", path.display());
        path
    })
    .clone()
}

/// The shipped manifest installed in `dir` with a copy of the library of
/// its own (so that the copy's process-wide state, its log callback among
/// it, is this test's).
pub fn install(dir: &Path) -> Manifest {
    let lib = dir.join("libobc_module_onnx.so");
    std::fs::copy(library(), &lib).unwrap();
    let text = std::fs::read_to_string(crate_dir().join("onnx.toml")).unwrap();
    let installed = text.replace(PACKAGED, &format!("library = \"{}\"", lib.display()));
    assert_ne!(text, installed, "the manifest's library line moved");
    let path = dir.join("onnx.toml");
    std::fs::write(&path, installed).unwrap();
    Manifest::load(&path).unwrap()
}

/// The module's log lines.
#[derive(Clone, Default)]
pub struct Log(Arc<Mutex<Vec<String>>>);

impl Log {
    pub fn sink(&self) -> LogSink {
        let lines = self.0.clone();
        Arc::new(move |_id: &str, level: LogLevel, msg: &str| {
            eprintln!("{level}: {msg}");
            lines.lock().unwrap().push(msg.to_string());
        })
    }

    pub fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }

    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

/// The module installed in `dir`, opened with `config`.
pub fn open(dir: &Path, config: serde_json::Value, log: &Log) -> Module {
    Module::open(&install(dir), &config, log.sink()).unwrap()
}

pub fn s(id: &str) -> StageId {
    StageId::new(id).unwrap()
}

/// A chain of `background` over host NV12 frames of `w x h` (BT.709,
/// limited range).
pub fn host_chain(w: u32, h: u32, params: &[(&str, &str)], bg: Option<HostBgr>) -> ChainSpec {
    ChainSpec {
        stages: vec![s("background")],
        kind: OBC_FRAME_HOST,
        input_format: OBC_FORMAT_NV12,
        input_size: (w, h),
        output_format: OBC_FORMAT_NV12,
        output_size: (w, h),
        matrix: OBC_MATRIX_BT709,
        chroma_siting: OBC_CHROMA_SITING_MPEG2,
        full_range: false,
        cuda: obc_cuda {
            context: std::ptr::null_mut(),
            stream: std::ptr::null_mut(),
        },
        params: params
            .iter()
            .map(|(k, v)| (s("background"), k.to_string(), Value::Str(v.to_string())))
            .collect(),
        background: bg.map(Arc::new),
    }
}

/// An NV12 image in one buffer: the Y rows then the UV rows, `pitch`
/// bytes apart.
#[derive(Clone)]
pub struct Nv12 {
    pub w: usize,
    pub h: usize,
    pub pitch: usize,
    pub data: Vec<u8>,
}

impl Nv12 {
    pub fn y(&self, x: usize, y: usize) -> u8 {
        self.data[y * self.pitch + x]
    }

    /// The host frame over the buffer.
    pub fn frame(&mut self, flags: u32) -> obc_frame {
        let mut f = obc_frame::empty();
        f.kind = OBC_FRAME_HOST;
        f.format = OBC_FORMAT_NV12;
        f.width = self.w as u32;
        f.height = self.h as u32;
        f.n_planes = 2;
        let base = self.data.as_mut_ptr() as u64;
        f.planes[0] = obc_plane {
            ptr_or_fd: base,
            pitch: self.pitch as u64,
            offset: 0,
        };
        f.planes[1] = obc_plane {
            ptr_or_fd: base,
            pitch: self.pitch as u64,
            offset: (self.pitch * self.h) as u64,
        };
        f.flags = flags;
        f
    }
}

/// BT.709 limited-range Y'CbCr of 8-bit RGB.
pub fn yuv(r: f32, g: f32, b: f32) -> [f32; 3] {
    let (r, g, b) = (r / 255.0, g / 255.0, b / 255.0);
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let cb = (b - y) / 1.8556;
    let cr = (r - y) / 1.5748;
    [16.0 + 219.0 * y, 128.0 + 224.0 * cb, 128.0 + 224.0 * cr]
}

/// An RGB image (`w x h`, 3 bytes a pixel) scaled to cover `ow x oh` and
/// converted to NV12 with rows of `pitch` bytes.
pub fn nv12_of_rgb(rgb: &[u8], w: usize, h: usize, ow: usize, oh: usize, pitch: usize) -> Nv12 {
    let scale = (ow as f64 / w as f64).max(oh as f64 / h as f64);
    let (ox, oy) = (
        (w as f64 - ow as f64 / scale) / 2.0,
        (h as f64 - oh as f64 / scale) / 2.0,
    );
    let px = |x: usize, y: usize| -> [f32; 3] {
        let sx = ((ox + (x as f64 + 0.5) / scale) as usize).min(w - 1);
        let sy = ((oy + (y as f64 + 0.5) / scale) as usize).min(h - 1);
        let i = 3 * (sy * w + sx);
        yuv(
            f32::from(rgb[i]),
            f32::from(rgb[i + 1]),
            f32::from(rgb[i + 2]),
        )
    };
    let mut data = vec![0u8; pitch * (oh + oh / 2)];
    for y in 0..oh {
        for x in 0..ow {
            data[y * pitch + x] = px(x, y)[0].round() as u8;
        }
    }
    for cy in 0..oh / 2 {
        for cx in 0..ow / 2 {
            let p = px(2 * cx, 2 * cy);
            data[(oh + cy) * pitch + 2 * cx] = p[1].round() as u8;
            data[(oh + cy) * pitch + 2 * cx + 1] = p[2].round() as u8;
        }
    }
    Nv12 {
        w: ow,
        h: oh,
        pitch,
        data,
    }
}

/// A PNG decoded to 8-bit RGB: `(pixels, width, height)`.
pub fn read_png(path: &Path) -> (Vec<u8>, usize, usize) {
    let mut d = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()));
    d.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut r = d.read_info().unwrap();
    let mut buf = vec![0; r.output_buffer_size().unwrap()];
    let info = r.next_frame(&mut buf).unwrap();
    let (w, h) = (info.width as usize, info.height as usize);
    let rgb = match info.color_type {
        png::ColorType::Rgb => buf[..w * h * 3].to_vec(),
        png::ColorType::Rgba => buf[..w * h * 4]
            .chunks(4)
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect(),
        png::ColorType::Grayscale => buf[..w * h].iter().flat_map(|&g| [g, g, g]).collect(),
        other => panic!("{other:?}"),
    };
    (rgb, w, h)
}

/// A synthetic scene: a lit head and shoulders over a textured room.
pub fn synthetic_rgb(w: usize, h: usize) -> Vec<u8> {
    let mut rgb = vec![0u8; w * h * 3];
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
            let head = ((fx - 0.5) / 0.12).powi(2) + ((fy - 0.35) / 0.2).powi(2) < 1.0;
            let body = fy > 0.55 && (fx - 0.5).abs() < 0.28 - (1.0 - fy) * 0.2;
            let p = if head {
                [224, 172, 140]
            } else if body {
                [40, 60, 140]
            } else if (x / 16 + y / 16) % 2 == 0 {
                [200, 200, 190]
            } else {
                [90, 80, 70]
            };
            rgb[3 * (y * w + x)..3 * (y * w + x) + 3].copy_from_slice(&p);
        }
    }
    rgb
}

/// The ONNX Runtime library and model directory of the environment, or
/// why the tests that need them are skipped.
pub fn runtime_env() -> Result<(PathBuf, PathBuf), String> {
    let lib = std::env::var_os("ORT_DYLIB_PATH").filter(|v| !v.is_empty());
    let models = std::env::var_os("OBC_ONNX_MODEL_DIR").filter(|v| !v.is_empty());
    match (lib, models) {
        (Some(l), Some(m)) => Ok((l.into(), m.into())),
        _ => Err("ORT_DYLIB_PATH and OBC_ONNX_MODEL_DIR are not both set".into()),
    }
}

/// The catalogue's file names, and the names the spike's files have.
pub const SPIKE_NAMES: [(&str, &str); 4] = [
    (
        "selfie_segmentation_landscape.onnx",
        "ours_selfie_segmentation_landscape.onnx",
    ),
    (
        "pp_humansegv2_mobile_192x192.onnx",
        "ours_human_pp_humansegv2_mobile_192x192.onnx",
    ),
    (
        "pp_humansegv2_lite_192x192.onnx",
        "ours_human_pp_humansegv2_lite_192x192.onnx",
    ),
    ("modnet_photographic.onnx", "modnet_photographic.onnx"),
];

/// A directory with the catalogue's file names, linked to the files of
/// `models` (under either name).
pub fn model_dir(models: &Path, into: &Path) -> PathBuf {
    let dir = into.join("models");
    std::fs::create_dir_all(&dir).unwrap();
    for (name, spike) in SPIKE_NAMES {
        let src = [models.join(name), models.join(spike)]
            .into_iter()
            .find(|p| p.is_file());
        if let Some(src) = src {
            std::os::unix::fs::symlink(&src, dir.join(name)).unwrap();
        }
    }
    dir
}

/// The test image: `$OBC_ONNX_TEST_IMAGE`, else the spike's
/// `$OBC_ONNX_MODEL_DIR/../images/c2_dark.png` when it is there, else a
/// synthetic scene of 1280x720.
pub fn test_image() -> (Vec<u8>, usize, usize) {
    let from_env = std::env::var_os("OBC_ONNX_TEST_IMAGE").map(PathBuf::from);
    let spike = std::env::var_os("OBC_ONNX_MODEL_DIR")
        .map(|d| PathBuf::from(d).join("../images/c2_dark.png"));
    match from_env.or(spike).filter(|p| p.is_file()) {
        Some(p) => {
            eprintln!("test image: {}", p.display());
            read_png(&p)
        }
        None => {
            eprintln!("test image: synthetic");
            (synthetic_rgb(1280, 720), 1280, 720)
        }
    }
}
