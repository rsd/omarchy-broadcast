//! A custom CUDA kernel: a byte lookup table over a pitched 8-bit
//! plane ([`Lut8`]).
//!
//! The kernel ships as PTX text (`kernels/lut8.ptx`, generated from
//! `kernels/lut8.cu`), embedded in the binary and compiled for the GPU by
//! the driver when it is loaded (`cuModuleLoadData`). Nothing here needs
//! the CUDA toolkit: the build never runs nvcc. The PTX is pinned to ISA
//! `.version 7.0` and `.target sm_75`, so any driver from 570.26 on (the
//! Maxine floor) loads it on any GPU from Turing on. Regenerating it is a
//! manual step documented at the top of `kernels/lut8.cu`.

use std::ffi::{CStr, c_void};

use crate::driver::{CuFunction, CuModule};
use crate::{CuDevicePtr, CuStream, GpuError, driver};

/// The embedded PTX of `obc_lut8_2d`, NUL-terminated for `cuModuleLoadData`.
pub const LUT8_PTX: &str = concat!(include_str!("kernels/lut8.ptx"), "\0");

/// The kernel's entry point in [`LUT8_PTX`].
pub const LUT8_ENTRY: &CStr = c"obc_lut8_2d";

/// The thread block every [`Lut8::launch`] uses: 32 x 8 threads, one warp
/// per row segment.
pub const LUT8_BLOCK: (u32, u32) = (32, 8);

/// `CUDA_ERROR_JIT_COMPILER_NOT_FOUND`.
const CUDA_ERROR_JIT_COMPILER_NOT_FOUND: i32 = 221;
/// `CUDA_ERROR_UNSUPPORTED_PTX_VERSION`.
const CUDA_ERROR_UNSUPPORTED_PTX_VERSION: i32 = 222;

/// What a failed `cuModuleLoadData` most likely means, for the two causes a
/// user can fix; `None` for anything else.
pub(crate) fn module_load_hint(code: i32) -> Option<&'static str> {
    match code {
        CUDA_ERROR_UNSUPPORTED_PTX_VERSION => Some(
            "the NVIDIA driver is older than this build supports: it cannot load the \
             effect kernel's PTX (ISA 7.0); install driver 570.26 or newer",
        ),
        CUDA_ERROR_JIT_COMPILER_NOT_FOUND => Some(
            "the driver's PTX compiler libnvidia-ptxjitcompiler.so.1 is missing; \
             install the package of your NVIDIA driver that ships it (on Arch: nvidia-utils)",
        ),
        _ => None,
    }
}

/// The `obc_lut8_2d` kernel, loaded into the context that was current when
/// it was created: `dst[y][x] = lut[src[y][x]]` over a `width x height`
/// rectangle of 8-bit pixels, rows `pitch` bytes apart. The bytes between
/// `width` and each pitch are neither read nor written.
///
/// The module is unloaded on drop, which must happen with the loading
/// context current (as for [`crate::DeviceBuffer`], [`crate::Stream`] and
/// [`crate::Event`], the owner keeps that context pushed around every call
/// and around drop), after the work that uses it has completed.
pub struct Lut8 {
    module: CuModule,
    function: CuFunction,
}

// SAFETY: a module and its functions belong to a context, not to a thread;
// they may be used and unloaded from any thread that has that context
// current.
unsafe impl Send for Lut8 {}

impl Lut8 {
    /// Loads the embedded kernel into the current context. The driver
    /// compiles the PTX for the GPU here (cached by the driver between
    /// runs), so call it while nothing is waiting for a frame.
    pub fn load() -> Result<Lut8, GpuError> {
        let ptx = CStr::from_bytes_with_nul(LUT8_PTX.as_bytes())
            .expect("the embedded PTX is NUL-terminated and has no inner NUL");
        Lut8::load_ptx(ptx)
    }

    /// Loads `ptx`, which must define [`LUT8_ENTRY`] with the parameters of
    /// `kernels/lut8.cu`. For tests that need a load to fail with a real
    /// driver error; use [`Lut8::load`].
    #[doc(hidden)]
    pub fn load_ptx(ptx: &CStr) -> Result<Lut8, GpuError> {
        let d = driver()?;
        let mut module: CuModule = std::ptr::null_mut();
        // SAFETY: `module` is a valid out-pointer; `ptx` is a NUL-terminated
        // image that outlives the call (the driver copies what it needs).
        let rc = unsafe { (d.cuModuleLoadData)(&mut module, ptx.as_ptr().cast::<c_void>()) };
        if let Err(mut e) = d.check("cuModuleLoadData", rc) {
            if let (GpuError::Call { description, .. }, Some(hint)) = (&mut e, module_load_hint(rc))
            {
                description.push_str("; ");
                description.push_str(hint);
            }
            return Err(e);
        }
        // Owned from here: an error below unloads the module.
        let mut kernel = Lut8 {
            module,
            function: std::ptr::null_mut(),
        };
        // SAFETY: `kernel.function` is a valid out-pointer; the module is
        // live; the entry name is NUL-terminated and static.
        d.check("cuModuleGetFunction", unsafe {
            (d.cuModuleGetFunction)(&mut kernel.function, kernel.module, LUT8_ENTRY.as_ptr())
        })?;
        Ok(kernel)
    }

    /// Enqueues `dst[y][x] = lut[src[y][x]]` for `x < width`, `y < height`
    /// on `stream`, in 32 x 8 thread blocks. Returns once the launch is
    /// queued: a fault while the kernel runs surfaces at the next
    /// synchronisation of `stream`.
    ///
    /// # Safety
    /// With the kernel's context current: `src .. src + src_pitch *
    /// (height - 1) + width` and the same rectangle at `dst` with
    /// `dst_pitch` must lie inside live device allocations of that context,
    /// `lut` must point at 256 readable device bytes, `dst` must not overlap
    /// the source or the table, and `stream` must be null or a live stream
    /// of the context. Nothing else may write those bytes until the kernel
    /// has run.
    #[allow(clippy::too_many_arguments)] // mirrors the kernel's parameters
    pub unsafe fn launch(
        &self,
        src: CuDevicePtr,
        src_pitch: usize,
        dst: CuDevicePtr,
        dst_pitch: usize,
        width: u32,
        height: u32,
        lut: CuDevicePtr,
        stream: CuStream,
    ) -> Result<(), GpuError> {
        // SAFETY: the caller's contract, passed on.
        unsafe {
            self.launch_with_block(
                src, src_pitch, dst, dst_pitch, width, height, lut, stream, LUT8_BLOCK,
            )
        }
    }

    /// [`Lut8::launch`] with a chosen thread block. For tests that need a
    /// launch to fail with a real driver error (a block of more than 1024
    /// threads); use [`Lut8::launch`].
    ///
    /// # Safety
    /// As [`Lut8::launch`].
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)] // mirrors the kernel's parameters
    pub unsafe fn launch_with_block(
        &self,
        src: CuDevicePtr,
        src_pitch: usize,
        dst: CuDevicePtr,
        dst_pitch: usize,
        width: u32,
        height: u32,
        lut: CuDevicePtr,
        stream: CuStream,
        block: (u32, u32),
    ) -> Result<(), GpuError> {
        if width == 0 || height == 0 || src_pitch < width as usize || dst_pitch < width as usize {
            return Err(GpuError::InvalidArgument(format!(
                "LUT over {width}x{height} with pitches {src_pitch} and {dst_pitch}"
            )));
        }
        if block.0 == 0 || block.1 == 0 {
            return Err(GpuError::InvalidArgument(format!(
                "empty thread block {}x{}",
                block.0, block.1
            )));
        }
        let d = driver()?;
        // The kernel's parameters, in order and with the PTX's widths:
        // u64 src, u64 src_pitch, u64 dst, u64 dst_pitch, u32 width,
        // u32 height, u64 lut.
        let mut a_src: u64 = src;
        let mut a_src_pitch: u64 = src_pitch as u64;
        let mut a_dst: u64 = dst;
        let mut a_dst_pitch: u64 = dst_pitch as u64;
        let mut a_width: u32 = width;
        let mut a_height: u32 = height;
        let mut a_lut: u64 = lut;
        let mut params: [*mut c_void; 7] = [
            (&raw mut a_src).cast(),
            (&raw mut a_src_pitch).cast(),
            (&raw mut a_dst).cast(),
            (&raw mut a_dst_pitch).cast(),
            (&raw mut a_width).cast(),
            (&raw mut a_height).cast(),
            (&raw mut a_lut).cast(),
        ];
        let grid = (width.div_ceil(block.0), height.div_ceil(block.1));
        // SAFETY: the function is live in the current context (caller);
        // `params` holds one pointer per kernel parameter, each to a local
        // of the parameter's width, all alive for the call (the driver
        // copies the values at launch); no extra options. The caller
        // guarantees the memory the kernel touches and the stream.
        d.check("cuLaunchKernel", unsafe {
            (d.cuLaunchKernel)(
                self.function,
                grid.0,
                grid.1,
                1,
                block.0,
                block.1,
                1,
                0,
                stream,
                params.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        })
    }
}

impl Drop for Lut8 {
    fn drop(&mut self) {
        if let Ok(d) = driver() {
            // SAFETY: `self.module` was loaded by us and is unloaded once,
            // with its context current (type contract).
            unsafe { (d.cuModuleUnload)(self.module) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The PTX lines from the entry point's `(` to its `)`.
    fn entry_params() -> Vec<&'static str> {
        let start = LUT8_PTX
            .find(".visible .entry obc_lut8_2d(")
            .expect("the entry point is declared");
        let rest = &LUT8_PTX[start..];
        let end = rest.find(')').expect("the parameter list ends");
        rest[..end]
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with(".param"))
            .collect()
    }

    #[test]
    fn the_embedded_ptx_is_pinned_to_isa_7_0_and_sm_75() {
        // A regeneration with a newer nvcc without the pin would raise the
        // driver floor above 570.26 (kernels/lut8.cu has the procedure).
        let directives: Vec<&str> = LUT8_PTX
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with(".version") || l.starts_with(".target"))
            .collect();
        assert_eq!(directives, [".version 7.0", ".target sm_75"]);
        assert!(
            LUT8_PTX.lines().any(|l| l.trim() == ".address_size 64"),
            "64-bit addressing"
        );
    }

    #[test]
    fn the_embedded_ptx_is_one_nul_terminated_string() {
        assert!(LUT8_PTX.ends_with('\0'));
        assert_eq!(LUT8_PTX.matches('\0').count(), 1);
        assert!(CStr::from_bytes_with_nul(LUT8_PTX.as_bytes()).is_ok());
    }

    #[test]
    fn the_entry_point_takes_the_seven_parameters_launch_passes() {
        assert_eq!(LUT8_ENTRY.to_str().unwrap(), "obc_lut8_2d");
        // src, src_pitch, dst, dst_pitch, width, height, lut: the order and
        // widths `launch_with_block` builds its parameter array with.
        let want = ["u64", "u64", "u64", "u64", "u32", "u32", "u64"];
        let got = entry_params();
        assert_eq!(got.len(), want.len(), "{got:?}");
        for (i, (line, ty)) in got.iter().zip(want).enumerate() {
            assert_eq!(
                line.trim_end_matches(','),
                format!(".param .{ty} obc_lut8_2d_param_{i}"),
                "parameter {i}"
            );
        }
        assert_eq!(
            LUT8_PTX.matches(".entry").count(),
            1,
            "exactly one kernel in the module"
        );
    }

    #[test]
    fn the_block_fits_every_gpu() {
        let threads = LUT8_BLOCK.0 * LUT8_BLOCK.1;
        assert!(threads <= 1024 && LUT8_BLOCK.0.is_multiple_of(32));
    }

    #[test]
    fn module_load_failures_explain_the_fixable_causes() {
        assert!(module_load_hint(222).unwrap().contains("570.26"));
        assert!(
            module_load_hint(221)
                .unwrap()
                .contains("libnvidia-ptxjitcompiler.so.1")
        );
        assert!(module_load_hint(218).is_none());
    }

    /// Assembles the committed PTX for the oldest and the newest GPU
    /// generation Maxine supports. `PTXAS` names the assembler (default:
    /// `ptxas` on `PATH`, then `/opt/cuda/bin/ptxas`).
    #[test]
    #[ignore = "needs ptxas (CUDA toolkit)"]
    fn ptxas_assembles_the_committed_ptx_for_sm_75_and_sm_120() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/kernels");
        let candidates: Vec<String> = match std::env::var("PTXAS") {
            Ok(p) if !p.is_empty() => vec![p],
            _ => vec!["ptxas".into(), "/opt/cuda/bin/ptxas".into()],
        };
        let ptxas = candidates
            .iter()
            .find(|c| {
                std::process::Command::new(c)
                    .arg("--version")
                    .output()
                    .is_ok_and(|o| o.status.success())
            })
            .unwrap_or_else(|| panic!("no ptxas found (tried {candidates:?}); set PTXAS"));
        for file in ["lut8.ptx", "pq_to_sdr.ptx"] {
            let path = format!("{dir}/{file}");
            for arch in ["sm_75", "sm_120"] {
                let out = std::process::Command::new(ptxas)
                    .args([&format!("-arch={arch}"), "-o", "/dev/null", &path])
                    .output()
                    .unwrap();
                assert!(
                    out.status.success(),
                    "ptxas -arch={arch} {file} failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
        }
    }
}
