//! Safe views of the audio structures of ABI 1.2: [`obc_audio_request`]
//! ([`AudioRequest`]), [`obc_audio_buffer`] as input ([`AudioBuffer`]) and
//! as output ([`AudioBufferMut`]), and the load report
//! ([`AudioLoadReport`]).

use std::marker::PhantomData;

use obc_module_abi::{
    OBC_AUDIO_BUFFER_SIZE_1_2, OBC_AUDIO_F32_PLANAR, OBC_AUDIO_FLAG_DISCONT,
    OBC_AUDIO_FLAG_NEW_SOURCE, OBC_AUDIO_REQUEST_SIZE_1_2, OBC_MAX_AUDIO_CHANNELS,
    obc_audio_buffer, obc_audio_request,
};

use crate::request::stage_ids;
use crate::{Error, Param, Value};

/// The block format an audio chain is loaded for: everything of an
/// [`AudioRequest`] but its stages and parameters. A load that keeps the
/// format of the loaded chain is a switch; one with another format is a
/// reload ([`crate::AudioInstance::load_audio`]).
///
/// The SDK makes it ([`AudioRequest::format`]); it may gain fields in a
/// later release, as the request does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct AudioFormat {
    /// Samples per second.
    pub sample_rate: u32,
    /// The channels of the input blocks.
    pub channels_in: u32,
    /// The channels of the output blocks.
    pub channels_out: u32,
    /// The samples per channel per block the host proposed.
    pub frame_samples: u32,
}

// The views read the whole structures once `struct_size` reaches the base
// size: sound while no field has been appended. A field appended in a later
// minor version is read only when it fits in `struct_size`.
const _: () = assert!(size_of::<obc_audio_request>() == OBC_AUDIO_REQUEST_SIZE_1_2 as usize);
const _: () = assert!(size_of::<obc_audio_buffer>() == OBC_AUDIO_BUFFER_SIZE_1_2 as usize);

/// The audio chain [`crate::AudioInstance::load_audio`] is asked to load: a
/// checked view of an [`obc_audio_request`], valid for the duration of the
/// call.
#[derive(Debug, Clone)]
pub struct AudioRequest<'a> {
    raw: &'a obc_audio_request,
    stages: Vec<&'a str>,
    params: Vec<Param<'a>>,
}

impl<'a> AudioRequest<'a> {
    /// A view of the request at `ptr`, checked: refused when null, smaller
    /// than the request of ABI 1.2 ([`OBC_AUDIO_REQUEST_SIZE_1_2`]), with a channel count of 0 or more
    /// than [`OBC_MAX_AUDIO_CHANNELS`], a rate or block size of 0, or when a
    /// stage id or a parameter is invalid.
    ///
    /// # Safety
    ///
    /// `ptr` is null or points to a readable request of at least the
    /// `struct_size` it states, whose arrays and strings are valid for
    /// `'a`.
    pub unsafe fn from_ptr(ptr: *const obc_audio_request) -> Result<Self, Error> {
        if ptr.is_null() {
            return Err(Error::invalid("audio request is null"));
        }
        // SAFETY: non-null and readable; only the first field is read
        // before the size is known.
        let size = unsafe { (*ptr).struct_size } as usize;
        if size < OBC_AUDIO_REQUEST_SIZE_1_2 as usize {
            return Err(Error::invalid(format!(
                "audio request struct_size {size} is smaller than {OBC_AUDIO_REQUEST_SIZE_1_2}"
            )));
        }
        // SAFETY: at least the base size, the size of `obc_audio_request`
        // (asserted above).
        let raw = unsafe { &*ptr };
        for (what, n) in [
            ("channels_in", raw.channels_in),
            ("channels_out", raw.channels_out),
        ] {
            if n == 0 || n as usize > OBC_MAX_AUDIO_CHANNELS {
                return Err(Error::invalid(format!(
                    "audio request {what} {n} is not 1 to {OBC_MAX_AUDIO_CHANNELS}"
                )));
            }
        }
        if raw.sample_rate == 0 || raw.frame_samples == 0 {
            return Err(Error::invalid(
                "audio request sample_rate and frame_samples must be non-zero",
            ));
        }
        // SAFETY: forwarded from the caller.
        let stages = unsafe { stage_ids(raw.stages, raw.n_stages) }?;
        // SAFETY: forwarded from the caller.
        let params = unsafe { Param::slice_from_raw(raw.params, raw.n_params) }?;
        Ok(AudioRequest {
            raw,
            stages,
            params,
        })
    }

    /// The underlying structure.
    pub fn raw(&self) -> &'a obc_audio_request {
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

    /// Samples per second.
    pub fn sample_rate(&self) -> u32 {
        self.raw.sample_rate
    }

    /// The channels of the input blocks.
    pub fn channels_in(&self) -> u32 {
        self.raw.channels_in
    }

    /// The channels of the output blocks.
    pub fn channels_out(&self) -> u32 {
        self.raw.channels_out
    }

    /// The samples per channel per block the host proposes (one of the
    /// manifest's `frame_samples`).
    pub fn frame_samples(&self) -> u32 {
        self.raw.frame_samples
    }

    /// The block format of the chain: a load whose format is that of the
    /// loaded chain is a switch.
    pub fn format(&self) -> AudioFormat {
        AudioFormat {
            sample_rate: self.raw.sample_rate,
            channels_in: self.raw.channels_in,
            channels_out: self.raw.channels_out,
            frame_samples: self.raw.frame_samples,
        }
    }
}

/// What [`crate::AudioInstance::load_audio`] reports.
///
/// Built with [`AudioLoadReport::new`] and the `with_` methods, or from
/// [`AudioLoadReport::default`] by assigning the fields: the structure may
/// gain fields in a later release, so it cannot be written as a literal
/// outside this crate.
///
/// ```
/// use obc_module_sdk::AudioLoadReport;
///
/// let report = AudioLoadReport::new(40.0)
///     .with_latency_samples(480)
///     .with_reused_model(true);
/// assert_eq!(report.frame_samples, None);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[non_exhaustive]
pub struct AudioLoadReport {
    /// How long the load took, in milliseconds.
    pub load_ms: f32,
    /// How long the warm-up took, in milliseconds, if there was one.
    pub warmup_ms: Option<f32>,
    /// The samples per channel per block the chain takes, when the module
    /// chose another of the manifest's `frame_samples` than the request's
    /// ([`AudioRequest::frame_samples`]); `None` takes the request's.
    pub frame_samples: Option<u32>,
    /// The chain's algorithmic delay in samples, its lookahead included.
    pub latency_samples: u32,
    /// Whether models of the previous load were reused.
    pub reused_model: bool,
}

impl AudioLoadReport {
    /// The report of a load that took `load_ms` milliseconds, with no
    /// warm-up, the request's block size, no delay and no reused model.
    pub fn new(load_ms: f32) -> Self {
        AudioLoadReport {
            load_ms,
            ..AudioLoadReport::default()
        }
    }

    /// Sets how long the warm-up took, in milliseconds: a number, or
    /// `None` for no warm-up.
    pub fn with_warmup_ms(mut self, warmup_ms: impl Into<Option<f32>>) -> Self {
        self.warmup_ms = warmup_ms.into();
        self
    }

    /// Sets the block size the chain takes
    /// ([`AudioLoadReport::frame_samples`]): a number, or `None` for the
    /// request's.
    pub fn with_frame_samples(mut self, frame_samples: impl Into<Option<u32>>) -> Self {
        self.frame_samples = frame_samples.into();
        self
    }

    /// Sets the chain's algorithmic delay in samples.
    pub fn with_latency_samples(mut self, latency_samples: u32) -> Self {
        self.latency_samples = latency_samples;
        self
    }

    /// Sets whether models of the previous load were reused.
    pub fn with_reused_model(mut self, reused_model: bool) -> Self {
        self.reused_model = reused_model;
        self
    }
}

/// The planes of a buffer at `raw`, checked against the format the
/// instance was loaded for: `channels` planes of `n_samples` samples.
fn check_buffer(
    raw: &obc_audio_buffer,
    what: &str,
    sample_rate: u32,
    channels: u32,
    n_samples: u32,
) -> Result<(), Error> {
    if raw.format != OBC_AUDIO_F32_PLANAR {
        return Err(Error::unsupported(format!(
            "{what}: audio format {} is not f32 planar",
            raw.format
        )));
    }
    if (raw.sample_rate, raw.channels, raw.n_samples) != (sample_rate, channels, n_samples) {
        return Err(Error::invalid(format!(
            "{what}: a block of {} Hz, {} channels, {} samples; the chain is loaded for \
             {sample_rate} Hz, {channels} channels, {n_samples} samples",
            raw.sample_rate, raw.channels, raw.n_samples
        )));
    }
    if let Some(c) = raw.planes[..channels as usize]
        .iter()
        .position(|p| p.is_null())
    {
        return Err(Error::invalid(format!("{what}: plane {c} is null")));
    }
    if let Some(c) = raw.planes[..channels as usize]
        .iter()
        .position(|p| !(*p as usize).is_multiple_of(align_of::<f32>()))
    {
        return Err(Error::invalid(format!("{what}: plane {c} is misaligned")));
    }
    Ok(())
}

/// The byte range of plane `p` of `n` samples.
fn range(p: *const f32, n: u32) -> (usize, usize) {
    let start = p as usize;
    (start, start + n as usize * size_of::<f32>())
}

fn overlap(a: (usize, usize), b: (usize, usize)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

/// A block of input audio ([`crate::AudioInstance::process_audio`]): a
/// checked, read-only view of an [`obc_audio_buffer`], valid for the
/// duration of the call.
#[derive(Debug)]
pub struct AudioBuffer<'a> {
    raw: &'a obc_audio_buffer,
}

impl<'a> AudioBuffer<'a> {
    /// The underlying structure.
    pub fn raw(&self) -> &'a obc_audio_buffer {
        self.raw
    }

    /// Samples per second.
    pub fn sample_rate(&self) -> u32 {
        self.raw.sample_rate
    }

    /// The number of channels (planes).
    pub fn channels(&self) -> usize {
        self.raw.channels as usize
    }

    /// Samples per channel in this block.
    pub fn n_samples(&self) -> usize {
        self.raw.n_samples as usize
    }

    /// The capture time of the first sample in nanoseconds, if known.
    pub fn pts_ns(&self) -> Option<i64> {
        (self.raw.pts_ns >= 0).then_some(self.raw.pts_ns)
    }

    /// The `OBC_AUDIO_FLAG_*` bits.
    pub fn flags(&self) -> u32 {
        self.raw.flags
    }

    /// Whether this is the first block of a new source
    /// ([`OBC_AUDIO_FLAG_NEW_SOURCE`]): temporal state should not carry
    /// over.
    pub fn is_new_source(&self) -> bool {
        self.raw.flags & OBC_AUDIO_FLAG_NEW_SOURCE != 0
    }

    /// Whether a gap precedes this block ([`OBC_AUDIO_FLAG_DISCONT`]).
    pub fn is_discont(&self) -> bool {
        self.raw.flags & OBC_AUDIO_FLAG_DISCONT != 0
    }

    /// The samples of channel `channel`, `None` past the last channel.
    pub fn plane(&self, channel: usize) -> Option<&'a [f32]> {
        if channel >= self.channels() {
            return None;
        }
        // SAFETY: checked non-null and aligned at construction; the host
        // owns `n_samples` readable samples there for the call, which no
        // output plane overlaps (checked at construction).
        Some(unsafe { std::slice::from_raw_parts(self.raw.planes[channel], self.n_samples()) })
    }
}

/// A block of output audio ([`crate::AudioInstance::process_audio`]): a
/// checked view of an [`obc_audio_buffer`] whose planes the module writes,
/// valid for the duration of the call.
#[derive(Debug)]
pub struct AudioBufferMut<'a> {
    raw: *mut obc_audio_buffer,
    channels: usize,
    n_samples: usize,
    _life: PhantomData<&'a mut obc_audio_buffer>,
}

impl<'a> AudioBufferMut<'a> {
    /// Samples per second.
    pub fn sample_rate(&self) -> u32 {
        // SAFETY: valid for `'a` (construction).
        unsafe { (*self.raw).sample_rate }
    }

    /// The number of channels (planes).
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Samples per channel in this block.
    pub fn n_samples(&self) -> usize {
        self.n_samples
    }

    /// The samples of channel `channel`, to write; `None` past the last
    /// channel. Every sample of every plane is written by the end of the
    /// call.
    pub fn plane_mut(&mut self, channel: usize) -> Option<&mut [f32]> {
        if channel >= self.channels {
            return None;
        }
        // SAFETY: valid for `'a`; checked non-null, aligned and apart from
        // every other plane at construction; `&mut self` keeps one slice
        // alive at a time.
        unsafe {
            let p = (*self.raw).planes[channel];
            Some(std::slice::from_raw_parts_mut(p, self.n_samples))
        }
    }

    /// Sets how likely the block holds speech (0.0 to 1.0, clamped), or
    /// unknown (`None`, the default).
    pub fn set_voice_probability(&mut self, p: Option<f32>) {
        let v = p.map_or(-1.0, |p| if p.is_nan() { -1.0 } else { p.clamp(0.0, 1.0) });
        // SAFETY: valid and writable for `'a` (construction).
        unsafe { (*self.raw).voice_probability = v };
    }
}

/// What a loaded audio chain expects of its blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AudioLoaded {
    /// The format of the request that loaded it.
    pub format: AudioFormat,
    /// The block size chosen.
    pub frame_samples: u32,
}

/// Views of the input and output blocks at `input` and `output`, checked
/// against `loaded`: refused when null, smaller than the buffer of ABI
/// 1.2 ([`OBC_AUDIO_BUFFER_SIZE_1_2`]), of another format, rate, channel count or block size, with a
/// null or misaligned plane, or when two planes overlap (the input's with
/// the output's, or two of the output's). The output's voice probability
/// is set to unknown.
///
/// # Safety
///
/// `input` and `output` are null or point to readable buffers of at least
/// the `struct_size` they state (`output` writable), whose planes hold
/// `n_samples` samples each, valid for `'a` and accessed by no one else
/// meanwhile.
pub(crate) unsafe fn buffers<'a>(
    input: *const obc_audio_buffer,
    output: *mut obc_audio_buffer,
    loaded: AudioLoaded,
) -> Result<(AudioBuffer<'a>, AudioBufferMut<'a>), Error> {
    for (what, p) in [("input", input), ("output", output.cast_const())] {
        if p.is_null() {
            return Err(Error::invalid(format!("{what}: the audio buffer is null")));
        }
        // SAFETY: non-null and readable; only the first field is read
        // before the size is known.
        let size = unsafe { (*p).struct_size } as usize;
        if size < OBC_AUDIO_BUFFER_SIZE_1_2 as usize {
            return Err(Error::invalid(format!(
                "{what}: audio buffer struct_size {size} is smaller than {OBC_AUDIO_BUFFER_SIZE_1_2}"
            )));
        }
    }
    // SAFETY: at least the base size, the size of `obc_audio_buffer`
    // (asserted above), by the caller's contract valid for `'a`.
    let inp = unsafe { &*input };
    // SAFETY: as above; read through a copy, written through the pointer.
    let out = unsafe { *output };
    let f = loaded.format;
    check_buffer(
        inp,
        "input",
        f.sample_rate,
        f.channels_in,
        loaded.frame_samples,
    )?;
    check_buffer(
        &out,
        "output",
        f.sample_rate,
        f.channels_out,
        loaded.frame_samples,
    )?;
    let n = loaded.frame_samples;
    let ins: Vec<(usize, usize)> = inp.planes[..f.channels_in as usize]
        .iter()
        .map(|p| range(*p, n))
        .collect();
    let outs: Vec<(usize, usize)> = out.planes[..f.channels_out as usize]
        .iter()
        .map(|p| range(*p, n))
        .collect();
    for (i, o) in outs.iter().enumerate() {
        if ins.iter().any(|r| overlap(*r, *o)) {
            return Err(Error::invalid(format!(
                "output: plane {i} overlaps the input"
            )));
        }
        if outs[..i].iter().any(|r| overlap(*r, *o)) {
            return Err(Error::invalid(format!(
                "output: plane {i} overlaps another output plane"
            )));
        }
    }
    let mut output = AudioBufferMut {
        raw: output,
        channels: f.channels_out as usize,
        n_samples: n as usize,
        _life: PhantomData,
    };
    output.set_voice_probability(None);
    Ok((AudioBuffer { raw: inp }, output))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded(ch_in: u32, ch_out: u32, n: u32) -> AudioLoaded {
        AudioLoaded {
            format: AudioFormat {
                sample_rate: 48_000,
                channels_in: ch_in,
                channels_out: ch_out,
                frame_samples: n,
            },
            frame_samples: n,
        }
    }

    fn buffer(planes: &mut [Vec<f32>]) -> obc_audio_buffer {
        let mut b = obc_audio_buffer::empty();
        b.format = OBC_AUDIO_F32_PLANAR;
        b.sample_rate = 48_000;
        b.channels = planes.len() as u32;
        b.n_samples = planes.first().map_or(0, |p| p.len() as u32);
        for (i, p) in planes.iter_mut().enumerate() {
            b.planes[i] = p.as_mut_ptr();
        }
        b
    }

    #[test]
    fn a_block_reads_and_writes_through_the_views() {
        let mut a = vec![vec![0.5f32; 4]];
        let mut z = vec![vec![0.0f32; 4]];
        let mut inp = buffer(&mut a);
        inp.flags = OBC_AUDIO_FLAG_DISCONT;
        inp.pts_ns = 7;
        let mut out = buffer(&mut z);
        out.voice_probability = 0.25;
        // SAFETY: both buffers and their planes live across the views.
        let (i, mut o) = unsafe { buffers(&inp, &mut out, loaded(1, 1, 4)) }.unwrap();
        assert_eq!(
            (i.channels(), i.n_samples(), i.sample_rate()),
            (1, 4, 48_000)
        );
        assert!(i.is_discont() && !i.is_new_source());
        assert_eq!(i.pts_ns(), Some(7));
        assert!(i.plane(1).is_none());
        let src = i.plane(0).unwrap();
        let dst = o.plane_mut(0).unwrap();
        for (d, s) in dst.iter_mut().zip(src) {
            *d = s * 2.0;
        }
        o.set_voice_probability(Some(1.5));
        assert_eq!(z[0], [1.0; 4]);
        assert_eq!(out.voice_probability, 1.0, "clamped");
    }

    #[test]
    fn the_voice_probability_starts_unknown() {
        let mut a = vec![vec![0.0f32; 2]];
        let mut z = vec![vec![0.0f32; 2]];
        let inp = buffer(&mut a);
        let mut out = buffer(&mut z);
        out.voice_probability = 0.5;
        // SAFETY: as above.
        unsafe { buffers(&inp, &mut out, loaded(1, 1, 2)) }.unwrap();
        assert_eq!(out.voice_probability, -1.0);
    }

    #[test]
    fn bad_blocks_are_refused() {
        let mut a = vec![vec![0.0f32; 4]];
        let mut z = vec![vec![0.0f32; 4], vec![0.0f32; 4]];
        let inp = buffer(&mut a);
        let mut out = buffer(&mut z);
        // SAFETY (each call): the buffers and their planes live across it.
        let e = unsafe { buffers(&inp, &mut out, loaded(1, 1, 4)) }.unwrap_err();
        assert!(e.message().contains("2 channels"), "{e}");
        let e = unsafe { buffers(&inp, &mut out, loaded(1, 2, 8)) }.unwrap_err();
        assert!(e.message().contains("8 samples"), "{e}");
        // The output's planes over the input's.
        let mut same = inp;
        let e = unsafe { buffers(&inp, &mut same, loaded(1, 1, 4)) }.unwrap_err();
        assert!(e.message().contains("overlaps the input"), "{e}");
        // Two output planes over each other.
        let mut twice = out;
        twice.planes[1] = twice.planes[0];
        let e = unsafe { buffers(&inp, &mut twice, loaded(1, 2, 4)) }.unwrap_err();
        assert!(e.message().contains("another output plane"), "{e}");
        let mut null = out;
        null.planes[1] = std::ptr::null_mut();
        let e = unsafe { buffers(&inp, &mut null, loaded(1, 2, 4)) }.unwrap_err();
        assert!(e.message().contains("plane 1 is null"), "{e}");
        let mut other = inp;
        other.format = 9;
        let e = unsafe { buffers(&other, &mut out, loaded(1, 2, 4)) }.unwrap_err();
        assert_eq!(e.kind(), crate::ErrorKind::Unsupported);
        let mut small = inp;
        small.struct_size = 8;
        let e = unsafe { buffers(&small, &mut out, loaded(1, 2, 4)) }.unwrap_err();
        assert!(e.message().contains("struct_size 8"), "{e}");
        let e = unsafe { buffers(std::ptr::null(), &mut out, loaded(1, 2, 4)) }.unwrap_err();
        assert!(e.message().contains("null"), "{e}");
    }

    #[test]
    fn a_request_is_checked_and_read() {
        let ids = [c"mic-denoise".as_ptr(), c"mic-enhance".as_ptr()];
        let mut raw = obc_audio_request::empty();
        raw.stages = ids.as_ptr();
        raw.n_stages = 2;
        raw.sample_rate = 48_000;
        raw.channels_in = 1;
        raw.channels_out = 1;
        raw.frame_samples = 480;
        // SAFETY: the request and its strings live across the view.
        let req = unsafe { AudioRequest::from_ptr(&raw) }.unwrap();
        assert_eq!(req.stages(), ["mic-denoise", "mic-enhance"]);
        assert!(req.has_stage("mic-enhance"));
        assert_eq!(
            req.format(),
            AudioFormat {
                sample_rate: 48_000,
                channels_in: 1,
                channels_out: 1,
                frame_samples: 480
            }
        );
        let mut bad = raw;
        bad.channels_out = 9;
        // SAFETY: as above.
        assert!(unsafe { AudioRequest::from_ptr(&bad) }.is_err());
        let mut bad = raw;
        bad.frame_samples = 0;
        // SAFETY: as above.
        assert!(unsafe { AudioRequest::from_ptr(&bad) }.is_err());
        let mut bad = raw;
        bad.struct_size = 16;
        // SAFETY: as above.
        assert!(unsafe { AudioRequest::from_ptr(&bad) }.is_err());
        // SAFETY: null is refused.
        assert!(unsafe { AudioRequest::from_ptr(std::ptr::null()) }.is_err());
    }
}
