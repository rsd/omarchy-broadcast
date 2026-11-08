// The source of lut8.ptx, the kernel behind obc_gpu::Lut8.
//
// Applies a 256-entry byte table to a pitched 8-bit plane:
// dst[y][x] = lut[src[y][x]] for x < width, y < height. The bytes between
// width and the pitch are neither read nor written.
//
// lut8.ptx is committed and embedded in obc-gpu; the build never runs nvcc.
// To regenerate it after changing this file (CUDA toolkit on PATH):
//
//   nvcc -arch=compute_75 -ptx -o lut8.ptx lut8.cu
//   sed -i 's/^\.version .*/.version 7.0/' lut8.ptx
//   ptxas -arch=sm_75 -o /dev/null lut8.ptx
//   ptxas -arch=sm_120 -o /dev/null lut8.ptx
//
// The .version pin keeps the PTX loadable by every driver from 570.26 on
// (the Maxine SDK's floor, which accepts PTX ISA up to 8.7): a newer nvcc
// writes its own, newer ISA version, which would quietly raise the driver
// floor. The kernel uses only instructions that ISA 7.0 has; both ptxas
// runs must succeed. .target sm_75 (Turing) is the oldest GPU Maxine
// supports. The unit tests of obc-gpu check the header and the parameter
// list; `cargo test -p obc-gpu -- --ignored ptxas` runs the two ptxas checks.
extern "C" __global__ void obc_lut8_2d(const unsigned char* __restrict__ src, unsigned long long src_pitch,
                                       unsigned char* __restrict__ dst, unsigned long long dst_pitch,
                                       unsigned int width, unsigned int height,
                                       const unsigned char* __restrict__ lut)
{
    unsigned int x = blockIdx.x * blockDim.x + threadIdx.x;
    unsigned int y = blockIdx.y * blockDim.y + threadIdx.y;
    if (x >= width || y >= height) return;
    dst[y * dst_pitch + x] = __ldg(&lut[src[y * src_pitch + x]]);
}
