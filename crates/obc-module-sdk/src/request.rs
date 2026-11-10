//! Safe views of [`obc_chain_request`] and [`obc_param`], and the load
//! report.

use std::ffi::{CStr, c_char};

use obc_module_abi::{
    OBC_CHROMA_SITING_COSITED, OBC_CHROMA_SITING_JPEG, OBC_CHROMA_SITING_MPEG2, OBC_MATRIX_BT601,
    OBC_MATRIX_BT709, OBC_MATRIX_BT2020, OBC_VALUE_BOOL, OBC_VALUE_FLOAT, OBC_VALUE_INT,
    OBC_VALUE_STRING, obc_chain_request, obc_param,
};

use crate::{CudaHandles, Error, Frame, FrameKind, PixelFormat};

/// The value of a parameter, borrowed from the call.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value<'a> {
    /// A boolean (a `bool` setting).
    Bool(bool),
    /// An integer (an `int` setting).
    Int(i64),
    /// A floating-point number.
    Float(f64),
    /// A string (an `enum` setting's option id).
    Str(&'a str),
}

impl<'a> Value<'a> {
    /// The boolean, if it is one.
    pub fn as_bool(&self) -> Option<bool> {
        match *self {
            Value::Bool(b) => Some(b),
            _ => None,
        }
    }

    /// The integer, if it is one.
    pub fn as_int(&self) -> Option<i64> {
        match *self {
            Value::Int(i) => Some(i),
            _ => None,
        }
    }

    /// The number, if it is a float or an integer.
    pub fn as_float(&self) -> Option<f64> {
        match *self {
            Value::Float(f) => Some(f),
            Value::Int(i) => Some(i as f64),
            _ => None,
        }
    }

    /// The string, if it is one.
    pub fn as_str(&self) -> Option<&'a str> {
        match *self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// One parameter of one stage: the `stage` and `key` of a setting the
/// module described, and its value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Param<'a> {
    /// The stage id.
    pub stage: &'a str,
    /// The parameter key.
    pub key: &'a str,
    /// The value.
    pub value: Value<'a>,
}

/// A NUL-terminated UTF-8 string of the host, refused when null.
///
/// # Safety
///
/// `ptr` is null or a NUL-terminated string valid for `'a`.
unsafe fn host_str<'a>(ptr: *const c_char, what: &str) -> Result<&'a str, Error> {
    if ptr.is_null() {
        return Err(Error::invalid(format!("{what} is null")));
    }
    // SAFETY: non-null and NUL-terminated by the caller's contract.
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| Error::invalid(format!("{what} is not UTF-8")))
}

/// The `n` stage ids at `ids`, checked: refused when `ids` is null with
/// `n` non-zero, or an id is null or not UTF-8.
///
/// # Safety
///
/// `ids` is null or points to `n` readable pointers, each null or a
/// NUL-terminated string valid for `'a`.
pub(crate) unsafe fn stage_ids<'a>(
    ids: *const *const c_char,
    n: usize,
) -> Result<Vec<&'a str>, Error> {
    if n == 0 {
        return Ok(Vec::new());
    }
    if ids.is_null() {
        return Err(Error::invalid(format!("{n} stage ids at a null pointer")));
    }
    // SAFETY: `n` readable pointers by the caller's contract.
    let ids = unsafe { std::slice::from_raw_parts(ids, n) };
    ids.iter()
        // SAFETY: NUL-terminated strings valid for `'a`.
        .map(|&id| unsafe { host_str(id, "stage id") })
        .collect()
}

impl<'a> Param<'a> {
    /// A view of `raw`, checked: refused when a string is null or not
    /// UTF-8, or the value kind is unknown.
    ///
    /// # Safety
    ///
    /// The strings `raw` points to are NUL-terminated and valid for `'a`.
    pub unsafe fn from_raw(raw: &'a obc_param) -> Result<Self, Error> {
        // SAFETY: forwarded from the caller.
        let stage = unsafe { host_str(raw.stage, "parameter stage") }?;
        // SAFETY: as above.
        let key = unsafe { host_str(raw.key, "parameter key") }?;
        // SAFETY (each arm): the kind names the member the host wrote.
        let value = match raw.value.kind {
            OBC_VALUE_BOOL => Value::Bool(unsafe { raw.value.data.b } != 0),
            OBC_VALUE_INT => Value::Int(unsafe { raw.value.data.i }),
            OBC_VALUE_FLOAT => Value::Float(unsafe { raw.value.data.f }),
            OBC_VALUE_STRING => {
                Value::Str(unsafe { host_str(raw.value.data.s, "parameter value") }?)
            }
            other => {
                return Err(Error::invalid(format!(
                    "parameter {stage}.{key} has unknown value kind {other}"
                )));
            }
        };
        Ok(Param { stage, key, value })
    }

    /// Views of the `n` parameters at `ptr` (null allowed when `n` is 0).
    ///
    /// # Safety
    ///
    /// `ptr` points to `n` readable parameters whose strings are valid for
    /// `'a`.
    pub unsafe fn slice_from_raw(ptr: *const obc_param, n: usize) -> Result<Vec<Self>, Error> {
        if n == 0 {
            return Ok(Vec::new());
        }
        if ptr.is_null() {
            return Err(Error::invalid(format!("{n} parameters at a null pointer")));
        }
        // SAFETY: `n` readable parameters, by the caller's contract.
        let raw = unsafe { std::slice::from_raw_parts(ptr, n) };
        raw.iter()
            // SAFETY: forwarded from the caller.
            .map(|p| unsafe { Param::from_raw(p) })
            .collect()
    }

    /// The value of `stage`'s `key` in `params` (the last one when there
    /// are several).
    pub fn find(params: &[Param<'a>], stage: &str, key: &str) -> Option<Value<'a>> {
        params
            .iter()
            .rev()
            .find(|p| p.stage == stage && p.key == key)
            .map(|p| p.value)
    }
}

/// The YUV colour matrix of a chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColourMatrix {
    /// ITU-R BT.601.
    Bt601,
    /// ITU-R BT.709.
    Bt709,
    /// ITU-R BT.2020.
    Bt2020,
    /// A code this SDK does not know.
    Other(u32),
}

/// Where the chroma samples of a chain's frames sit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChromaSiting {
    /// MPEG-2: horizontally co-sited, vertically centred.
    Mpeg2,
    /// JPEG / MPEG-1: centred in both directions.
    Jpeg,
    /// Co-sited with the top-left luma sample.
    Cosited,
    /// A code this SDK does not know.
    Other(u32),
}

/// The chain [`crate::Instance::load`] is asked to load: a checked view of
/// an [`obc_chain_request`], valid for the duration of the call.
#[derive(Debug, Clone)]
pub struct ChainRequest<'a> {
    raw: &'a obc_chain_request,
    stages: Vec<&'a str>,
    params: Vec<Param<'a>>,
    background: Option<Frame<'a>>,
}

impl<'a> ChainRequest<'a> {
    /// A view of the request at `ptr`, checked: refused when null, smaller
    /// than this ABI version's request, or when a stage id, a parameter or
    /// the background frame is invalid.
    ///
    /// # Safety
    ///
    /// `ptr` is null or points to a readable request of at least the
    /// `struct_size` it states, whose arrays, strings and background frame
    /// are valid for `'a`.
    pub unsafe fn from_ptr(ptr: *const obc_chain_request) -> Result<Self, Error> {
        if ptr.is_null() {
            return Err(Error::invalid("chain request is null"));
        }
        // SAFETY: non-null and readable; only the first field is read
        // before the size is known.
        let size = unsafe { (*ptr).struct_size } as usize;
        if size < size_of::<obc_chain_request>() {
            return Err(Error::invalid(format!(
                "chain request struct_size {size} is smaller than {}",
                size_of::<obc_chain_request>()
            )));
        }
        // SAFETY: at least as large as `obc_chain_request`.
        let raw = unsafe { &*ptr };
        // SAFETY: forwarded from the caller.
        let stages = unsafe { stage_ids(raw.stages, raw.n_stages) }?;
        // SAFETY: forwarded from the caller.
        let params = unsafe { Param::slice_from_raw(raw.params, raw.n_params) }?;
        let background = if raw.background.is_null() {
            None
        } else {
            // SAFETY: forwarded from the caller.
            Some(unsafe { Frame::from_ptr(raw.background) }?)
        };
        Ok(ChainRequest {
            raw,
            stages,
            params,
            background,
        })
    }

    /// The underlying structure.
    pub fn raw(&self) -> &'a obc_chain_request {
        self.raw
    }

    /// The stage ids of the chain, in the order they run.
    pub fn stages(&self) -> &[&'a str] {
        &self.stages
    }

    /// Whether the chain has stage `id`.
    pub fn has_stage(&self, id: &str) -> bool {
        self.stages.contains(&id)
    }

    /// The load-time parameters of every stage of the chain.
    pub fn params(&self) -> &[Param<'a>] {
        &self.params
    }

    /// The load-time value of `stage`'s `key`.
    pub fn param(&self, stage: &str, key: &str) -> Option<Value<'a>> {
        Param::find(&self.params, stage, key)
    }

    /// The kind of the frames the instance will process.
    pub fn input_kind(&self) -> FrameKind {
        FrameKind::from_raw(self.raw.input_kind)
    }

    /// The format of the input frames.
    pub fn input_format(&self) -> PixelFormat {
        PixelFormat::from_raw(self.raw.input_format)
    }

    /// The `(width, height)` of the input frames.
    pub fn input_size(&self) -> (u32, u32) {
        (self.raw.input_width, self.raw.input_height)
    }

    /// The format of the output frames.
    pub fn output_format(&self) -> PixelFormat {
        PixelFormat::from_raw(self.raw.output_format)
    }

    /// The `(width, height)` of the output frames.
    pub fn output_size(&self) -> (u32, u32) {
        (self.raw.output_width, self.raw.output_height)
    }

    /// The YUV matrix.
    pub fn matrix(&self) -> ColourMatrix {
        match self.raw.matrix {
            OBC_MATRIX_BT601 => ColourMatrix::Bt601,
            OBC_MATRIX_BT709 => ColourMatrix::Bt709,
            OBC_MATRIX_BT2020 => ColourMatrix::Bt2020,
            other => ColourMatrix::Other(other),
        }
    }

    /// The chroma siting.
    pub fn chroma_siting(&self) -> ChromaSiting {
        match self.raw.chroma_siting {
            OBC_CHROMA_SITING_MPEG2 => ChromaSiting::Mpeg2,
            OBC_CHROMA_SITING_JPEG => ChromaSiting::Jpeg,
            OBC_CHROMA_SITING_COSITED => ChromaSiting::Cosited,
            other => ChromaSiting::Other(other),
        }
    }

    /// `true` for full range, `false` for limited range (any non-zero byte
    /// is full range).
    pub fn full_range(&self) -> bool {
        self.raw.full_range != 0
    }

    /// The CUDA handles of a CUDA chain (null for host chains).
    pub fn cuda(&self) -> CudaHandles {
        CudaHandles::from_raw(self.raw.cuda)
    }

    /// The background image of a replacement (a host BGR frame), valid
    /// only during the call.
    pub fn background(&self) -> Option<&Frame<'a>> {
        self.background.as_ref()
    }

    /// The frames of the chain: a load whose frames are those of the
    /// loaded chain is a switch.
    pub fn frames(&self) -> ChainFrames {
        ChainFrames {
            kind: self.input_kind(),
            input_format: self.input_format(),
            input_size: self.input_size(),
            output_format: self.output_format(),
            output_size: self.output_size(),
            matrix: self.matrix(),
            chroma_siting: self.chroma_siting(),
            full_range: self.full_range(),
            cuda: self.cuda(),
        }
    }
}

/// The frames a chain is loaded for: everything of a [`ChainRequest`] but
/// its stages, parameters and background. A load that keeps the frames of
/// the loaded chain is a switch; one with other frames is a reload
/// ([`crate::Instance::load`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainFrames {
    /// The kind of the frames.
    pub kind: FrameKind,
    /// The input format.
    pub input_format: PixelFormat,
    /// The input size.
    pub input_size: (u32, u32),
    /// The output format.
    pub output_format: PixelFormat,
    /// The output size.
    pub output_size: (u32, u32),
    /// The YUV matrix.
    pub matrix: ColourMatrix,
    /// The chroma siting.
    pub chroma_siting: ChromaSiting,
    /// Full range.
    pub full_range: bool,
    /// The CUDA handles.
    pub cuda: CudaHandles,
}

/// What [`crate::Instance::load`] reports.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LoadReport {
    /// How long the load took, in milliseconds.
    pub load_ms: f32,
    /// How long the warm-up took, in milliseconds, if there was one.
    pub warmup_ms: Option<f32>,
    /// Whether models of the previous load were reused.
    pub reused_model: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use obc_module_abi::{OBC_FORMAT_NV12, OBC_FRAME_HOST, obc_frame, obc_value};

    #[test]
    fn params_convert_and_are_found() {
        let raw = [
            obc_param {
                stage: c"relight".as_ptr(),
                key: c"light".as_ptr(),
                value: obc_value::from_str_ptr(c"warm".as_ptr()),
            },
            obc_param {
                stage: c"relight".as_ptr(),
                key: c"direction".as_ptr(),
                value: obc_value::from_int(-30),
            },
            obc_param {
                stage: c"denoise".as_ptr(),
                key: c"on".as_ptr(),
                value: obc_value::from_bool(true),
            },
        ];
        // SAFETY: static strings.
        let params = unsafe { Param::slice_from_raw(raw.as_ptr(), raw.len()) }.unwrap();
        assert_eq!(
            Param::find(&params, "relight", "light"),
            Some(Value::Str("warm"))
        );
        assert_eq!(
            Param::find(&params, "relight", "direction").and_then(|v| v.as_int()),
            Some(-30)
        );
        assert_eq!(
            Param::find(&params, "denoise", "on"),
            Some(Value::Bool(true))
        );
        assert_eq!(Param::find(&params, "denoise", "off"), None);
        assert_eq!(Value::Int(2).as_float(), Some(2.0));
        // SAFETY: null with zero entries is allowed.
        assert!(
            unsafe { Param::slice_from_raw(std::ptr::null(), 0) }
                .unwrap()
                .is_empty()
        );
        // SAFETY: refused before any read.
        assert!(unsafe { Param::slice_from_raw(std::ptr::null(), 1) }.is_err());
    }

    #[test]
    fn bad_params_are_refused() {
        let mut raw = obc_param {
            stage: std::ptr::null(),
            key: c"k".as_ptr(),
            value: obc_value::from_int(1),
        };
        // SAFETY: static strings or null.
        assert!(unsafe { Param::from_raw(&raw) }.is_err());
        raw.stage = c"s".as_ptr();
        raw.value.kind = 99;
        // SAFETY: as above.
        let e = unsafe { Param::from_raw(&raw) }.unwrap_err();
        assert!(e.message().contains("unknown value kind"), "{e}");
        raw.value = obc_value::from_str_ptr(c"\xff".as_ptr());
        // SAFETY: as above.
        assert!(
            unsafe { Param::from_raw(&raw) }
                .unwrap_err()
                .message()
                .contains("UTF-8")
        );
    }

    #[test]
    fn requests_convert() {
        let stages = [c"denoise".as_ptr(), c"relight".as_ptr()];
        let params = [obc_param {
            stage: c"relight".as_ptr(),
            key: c"light".as_ptr(),
            value: obc_value::from_str_ptr(c"cool".as_ptr()),
        }];
        let mut background = obc_frame::empty();
        background.kind = OBC_FRAME_HOST;
        let mut raw = obc_chain_request::empty();
        raw.stages = stages.as_ptr();
        raw.n_stages = stages.len();
        raw.params = params.as_ptr();
        raw.n_params = params.len();
        raw.input_kind = OBC_FRAME_HOST;
        raw.input_format = OBC_FORMAT_NV12;
        raw.input_width = 640;
        raw.input_height = 480;
        raw.output_format = OBC_FORMAT_NV12;
        raw.output_width = 1280;
        raw.output_height = 960;
        raw.matrix = OBC_MATRIX_BT709;
        raw.chroma_siting = OBC_CHROMA_SITING_MPEG2;
        // Any non-zero byte is true.
        raw.full_range = 2;
        raw.background = &background;
        // SAFETY: everything points to live locals and static strings.
        let req = unsafe { ChainRequest::from_ptr(&raw) }.unwrap();
        assert_eq!(req.stages(), ["denoise", "relight"]);
        assert!(req.has_stage("relight") && !req.has_stage("enhance"));
        assert_eq!(req.param("relight", "light"), Some(Value::Str("cool")));
        assert_eq!(req.input_kind(), FrameKind::Host);
        assert_eq!(req.input_format(), PixelFormat::Nv12);
        assert_eq!(req.input_size(), (640, 480));
        assert_eq!(req.output_size(), (1280, 960));
        assert_eq!(req.matrix(), ColourMatrix::Bt709);
        assert_eq!(req.chroma_siting(), ChromaSiting::Mpeg2);
        assert!(req.full_range());
        assert!(req.cuda().is_null());
        assert_eq!(req.background().map(|f| f.kind()), Some(FrameKind::Host));

        raw.struct_size = 8;
        // SAFETY: as above.
        let e = unsafe { ChainRequest::from_ptr(&raw) }.unwrap_err();
        assert!(e.message().contains("struct_size"), "{e}");
        raw.struct_size = size_of::<obc_chain_request>() as u32;
        raw.stages = std::ptr::null();
        // SAFETY: as above.
        assert!(unsafe { ChainRequest::from_ptr(&raw) }.is_err());
        // SAFETY: refused before any read.
        assert!(unsafe { ChainRequest::from_ptr(std::ptr::null()) }.is_err());
    }
}
