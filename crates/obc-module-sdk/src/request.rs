//! Safe views of [`obc_chain_request`] and [`obc_param`], and the load
//! report.

use std::ffi::{CStr, c_char};

use obc_module_abi::{
    OBC_CHAIN_REQUEST_SIZE_1_0, OBC_CHROMA_SITING_COSITED, OBC_CHROMA_SITING_JPEG,
    OBC_CHROMA_SITING_MPEG2, OBC_MATRIX_BT601, OBC_MATRIX_BT709, OBC_MATRIX_BT2020, OBC_VALUE_BOOL,
    OBC_VALUE_FLOAT, OBC_VALUE_INT, OBC_VALUE_STRING, obc_chain_request, obc_chroma_siting,
    obc_matrix, obc_param,
};

use crate::{CudaHandles, Error, Frame, FrameKind, PixelFormat};

/// The value of a parameter, borrowed from the call.
///
/// A later release may add variants, for value kinds of a later ABI minor
/// version: a `match` on it has a catch-all arm.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
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
///
/// It mirrors `obc_param`, which is frozen for the life of major 1
/// (the structure has no `struct_size` and never grows), so it is not
/// `#[non_exhaustive]`: a module may build one with a struct literal.
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
    /// UTF-8 ([`ErrorKind::Invalid`](crate::ErrorKind::Invalid)), or the
    /// value kind is one this SDK does not know
    /// ([`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported), as a
    /// module answers a constant it does not know).
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
                return Err(Error::unsupported(format!(
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
///
/// A later release may name more codes, which until then are `Other`: a
/// `match` on it has a catch-all arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
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

impl ColourMatrix {
    /// The matrix of an `OBC_MATRIX_*` code.
    pub fn from_raw(code: obc_matrix) -> Self {
        match code {
            OBC_MATRIX_BT601 => ColourMatrix::Bt601,
            OBC_MATRIX_BT709 => ColourMatrix::Bt709,
            OBC_MATRIX_BT2020 => ColourMatrix::Bt2020,
            other => ColourMatrix::Other(other),
        }
    }

    /// The `OBC_MATRIX_*` code.
    pub fn to_raw(self) -> obc_matrix {
        match self {
            ColourMatrix::Bt601 => OBC_MATRIX_BT601,
            ColourMatrix::Bt709 => OBC_MATRIX_BT709,
            ColourMatrix::Bt2020 => OBC_MATRIX_BT2020,
            ColourMatrix::Other(code) => code,
        }
    }
}

/// Where the chroma samples of a chain's frames sit.
///
/// A later release may name more codes, which until then are `Other`: a
/// `match` on it has a catch-all arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
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

// The view reads the whole structure once `struct_size` reaches the base
// size: sound while no field has been appended. A field appended in a later
// minor version is read only when it fits in `struct_size`.
const _: () = assert!(size_of::<obc_chain_request>() == OBC_CHAIN_REQUEST_SIZE_1_0 as usize);

impl ChromaSiting {
    /// The siting of an `OBC_CHROMA_SITING_*` code.
    pub fn from_raw(code: obc_chroma_siting) -> Self {
        match code {
            OBC_CHROMA_SITING_MPEG2 => ChromaSiting::Mpeg2,
            OBC_CHROMA_SITING_JPEG => ChromaSiting::Jpeg,
            OBC_CHROMA_SITING_COSITED => ChromaSiting::Cosited,
            other => ChromaSiting::Other(other),
        }
    }

    /// The `OBC_CHROMA_SITING_*` code.
    pub fn to_raw(self) -> obc_chroma_siting {
        match self {
            ChromaSiting::Mpeg2 => OBC_CHROMA_SITING_MPEG2,
            ChromaSiting::Jpeg => OBC_CHROMA_SITING_JPEG,
            ChromaSiting::Cosited => OBC_CHROMA_SITING_COSITED,
            ChromaSiting::Other(code) => code,
        }
    }
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
    /// than the request of ABI 1.0 ([`OBC_CHAIN_REQUEST_SIZE_1_0`]), or when a stage id, a parameter or
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
        if size < OBC_CHAIN_REQUEST_SIZE_1_0 as usize {
            return Err(Error::invalid(format!(
                "chain request struct_size {size} is smaller than {OBC_CHAIN_REQUEST_SIZE_1_0}"
            )));
        }
        // SAFETY: at least the base size, the size of `obc_chain_request`
        // (asserted above).
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
        ColourMatrix::from_raw(self.raw.matrix)
    }

    /// The chroma siting.
    pub fn chroma_siting(&self) -> ChromaSiting {
        ChromaSiting::from_raw(self.raw.chroma_siting)
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
///
/// The SDK makes it ([`ChainRequest::frames`]); it may gain fields in a
/// later release, as the request does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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
///
/// Built with [`LoadReport::new`] and the `with_` methods, or from
/// [`LoadReport::default`] by assigning the fields: the structure may gain
/// fields in a later release, so it cannot be written as a literal outside
/// this crate.
///
/// ```
/// use obc_module_sdk::LoadReport;
///
/// let report = LoadReport::new(12.5)
///     .with_warmup_ms(3.0)
///     .with_notice("TensorRT is not available: the chain runs on CUDA");
/// assert!(!report.reused_model);
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct LoadReport {
    /// How long the load took, in milliseconds.
    pub load_ms: f32,
    /// How long the warm-up took, in milliseconds, if there was one.
    pub warmup_ms: Option<f32>,
    /// Whether models of the previous load were reused.
    pub reused_model: bool,
    /// What the user should know of this load, which succeeded, as one
    /// line of text: a setting that could not be honoured, and what runs in
    /// its place (an engine chosen in the settings that cannot run, and the
    /// one the chain runs on instead). The host shows it while the chain
    /// runs, and forgets it at the next load that reports none. Handed to a
    /// host of ABI 1.5 or later only ([`obc_module_abi::obc_load_report`]'s
    /// `notice`); an older host never sees it. A NUL byte in it is
    /// replaced.
    pub notice: Option<String>,
}

impl LoadReport {
    /// The report of a load that took `load_ms` milliseconds, with no
    /// warm-up, no reused model and no notice.
    pub fn new(load_ms: f32) -> Self {
        LoadReport {
            load_ms,
            ..LoadReport::default()
        }
    }

    /// Sets how long the warm-up took, in milliseconds: a number, or
    /// `None` for no warm-up.
    pub fn with_warmup_ms(mut self, warmup_ms: impl Into<Option<f32>>) -> Self {
        self.warmup_ms = warmup_ms.into();
        self
    }

    /// Sets whether models of the previous load were reused.
    pub fn with_reused_model(mut self, reused_model: bool) -> Self {
        self.reused_model = reused_model;
        self
    }

    /// Sets the notice of the load ([`LoadReport::notice`]).
    pub fn with_notice(mut self, notice: impl Into<String>) -> Self {
        self.notice = Some(notice.into());
        self
    }
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
        // A constant the module does not know: unsupported, not invalid.
        assert_eq!(e.kind(), crate::ErrorKind::Unsupported);
        assert_eq!(e.status(), obc_module_abi::OBC_ERR_UNSUPPORTED);
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
    fn colour_codes_round_trip() {
        for code in [OBC_MATRIX_BT601, OBC_MATRIX_BT709, OBC_MATRIX_BT2020, 77] {
            assert_eq!(ColourMatrix::from_raw(code).to_raw(), code);
        }
        assert_eq!(ColourMatrix::from_raw(77), ColourMatrix::Other(77));
        for code in [
            OBC_CHROMA_SITING_MPEG2,
            OBC_CHROMA_SITING_JPEG,
            OBC_CHROMA_SITING_COSITED,
            77,
        ] {
            assert_eq!(ChromaSiting::from_raw(code).to_raw(), code);
        }
        assert_eq!(ChromaSiting::from_raw(77), ChromaSiting::Other(77));
    }

    #[test]
    fn load_reports_build() {
        let report = LoadReport::new(2.0)
            .with_warmup_ms(1.0)
            .with_reused_model(true)
            .with_notice("runs on CUDA");
        assert_eq!(
            (report.load_ms, report.warmup_ms, report.reused_model),
            (2.0, Some(1.0), true)
        );
        assert_eq!(report.notice.as_deref(), Some("runs on CUDA"));
        assert_eq!(LoadReport::new(0.0), LoadReport::default());
        assert_eq!(LoadReport::new(0.0).with_warmup_ms(None).warmup_ms, None);
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
