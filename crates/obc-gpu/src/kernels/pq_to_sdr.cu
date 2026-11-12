// The source of pq_to_sdr.ptx, the kernel behind obc_gpu::PqToSdr.
//
// Tone-maps a pitched RGB10A2 image (one little-endian 32-bit word per
// pixel: R in bits 0-9, G in 10-19, B in 20-29, A in 30-31; full-range
// codes, SMPTE ST 2084 (PQ) transfer, BT.2020 primaries) to a pitched RGBA8
// image with the sRGB transfer and BT.709 primaries, alpha 255. The bytes
// between the row and each pitch are neither read nor written.
//
// The steps are those of obc_gpu::tonemap::pq_to_sdr, the reference in Rust
// that the unit tests pin (see tonemap.rs for why each step is there):
//
//   1. each code / 1023, PQ EOTF to nits;
//   2. divided by `gain` (where the HDR source put SDR diffuse white);
//   3. BT.2020 to BT.709 linear matrix, negatives clipped;
//   4. BT.2390 EETF on the BT.709 luminance, from `peak / gain` to 100 nits,
//      the channels scaled by the luminance ratio;
//   5. no channel above 100 nits, then / 100 and the sRGB encode;
//   6. * 255, rounded to nearest even, packed R, G, B, 255.
//
// pq_to_sdr.ptx is committed and embedded in obc-gpu; the build never runs
// nvcc. To regenerate it after changing this file (CUDA toolkit on PATH):
//
//   nvcc -arch=compute_75 -ptx -fmad=false -o pq_to_sdr.ptx pq_to_sdr.cu
//   sed -i 's/^\.version .*/.version 7.0/' pq_to_sdr.ptx
//   ptxas -arch=sm_75 -o /dev/null pq_to_sdr.ptx
//   ptxas -arch=sm_120 -o /dev/null pq_to_sdr.ptx
//
// -fmad=false keeps every multiply and add separately rounded, as in the Rust
// reference, so the two agree to within one code value. The .version pin is
// the one of lut8.cu (the driver floor of the Maxine SDK, 570.26); both ptxas
// runs must succeed. `cargo test -p obc-gpu -- --ignored ptxas` runs them.

#define PQ_M1 (2610.0f / 16384.0f)
#define PQ_M2 (2523.0f / 4096.0f * 128.0f)
#define PQ_C1 (3424.0f / 4096.0f)
#define PQ_C2 (2413.0f / 4096.0f * 32.0f)
#define PQ_C3 (2392.0f / 4096.0f * 32.0f)
#define SDR_WHITE 100.0f

__device__ __forceinline__ float clamp01(float v) { return fminf(fmaxf(v, 0.0f), 1.0f); }

// SMPTE ST 2084 EOTF: a non-linear value in [0, 1] to nits, 0..10000.
__device__ float pq_eotf(float e)
{
    float p = powf(clamp01(e), 1.0f / PQ_M2);
    return 10000.0f * powf(fmaxf(p - PQ_C1, 0.0f) / (PQ_C2 - PQ_C3 * p), 1.0f / PQ_M1);
}

// The inverse of pq_eotf.
__device__ float pq_oetf(float nits)
{
    float y = powf(fminf(fmaxf(nits, 0.0f), 10000.0f) / 10000.0f, PQ_M1);
    return powf((PQ_C1 + PQ_C2 * y) / (1.0f + PQ_C3 * y), PQ_M2);
}

// ITU-R BT.2390 EETF (Hermite knee in the PQ domain, black at 0 on both
// sides) from a source peaking at `src_peak` nits to SDR_WHITE.
__device__ float eetf(float nits, float src_peak)
{
    float ps = pq_oetf(src_peak);
    float maxlum = pq_oetf(SDR_WHITE) / ps;
    float ks = 1.5f * maxlum - 0.5f;
    float e = pq_oetf(nits) / ps;
    if (e >= ks) {
        float t = clamp01((e - ks) / (1.0f - ks));
        float t2 = t * t;
        float t3 = t2 * t;
        e = (2.0f * t3 - 3.0f * t2 + 1.0f) * ks + (t3 - 2.0f * t2 + t) * (1.0f - ks)
            + (-2.0f * t3 + 3.0f * t2) * maxlum;
    }
    return pq_eotf(e * ps);
}

__device__ float srgb_oetf(float l)
{
    l = clamp01(l);
    return l <= 0.0031308f ? l * 12.92f : 1.055f * powf(l, 1.0f / 2.4f) - 0.055f;
}

__device__ unsigned int to_u8(float v)
{
    return (unsigned int)__float2int_rn(clamp01(v) * 255.0f);
}

extern "C" __global__ void obc_pq_to_sdr(const unsigned char* __restrict__ src, unsigned long long src_pitch,
                                         unsigned char* __restrict__ dst, unsigned long long dst_pitch,
                                         unsigned int width, unsigned int height,
                                         float gain, float peak)
{
    unsigned int x = blockIdx.x * blockDim.x + threadIdx.x;
    unsigned int y = blockIdx.y * blockDim.y + threadIdx.y;
    if (x >= width || y >= height) return;
    unsigned int p = ((const unsigned int*)(src + y * src_pitch))[x];

    float r = pq_eotf((float)(p & 1023u) / 1023.0f) / gain;
    float g = pq_eotf((float)((p >> 10) & 1023u) / 1023.0f) / gain;
    float b = pq_eotf((float)((p >> 20) & 1023u) / 1023.0f) / gain;

    float r7 = fmaxf(1.6605f * r + -0.5876f * g + -0.0728f * b, 0.0f);
    float g7 = fmaxf(-0.1246f * r + 1.1329f * g + -0.0083f * b, 0.0f);
    float b7 = fmaxf(-0.0182f * r + -0.1006f * g + 1.1187f * b, 0.0f);

    float lum = 0.2126f * r7 + 0.7152f * g7 + 0.0722f * b7;
    float scale = eetf(lum, peak / gain) / fmaxf(lum, 1e-4f);
    r7 *= scale;
    g7 *= scale;
    b7 *= scale;

    float limit = fmaxf(1.0f, fmaxf(r7, fmaxf(g7, b7)) / SDR_WHITE);
    r7 /= limit;
    g7 /= limit;
    b7 /= limit;

    unsigned int out = to_u8(srgb_oetf(r7 / SDR_WHITE))
                     | (to_u8(srgb_oetf(g7 / SDR_WHITE)) << 8)
                     | (to_u8(srgb_oetf(b7 / SDR_WHITE)) << 16)
                     | (255u << 24);
    ((unsigned int*)(dst + y * dst_pitch))[x] = out;
}
