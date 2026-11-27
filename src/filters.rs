use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// Supported individual filter types in a pipeline
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilterType {
    /// First-order delta encoding
    Delta,
    /// Byte-wise shuffle
    ByteShuffle,
    /// Bit-wise shuffle (higher compression for binary / boolean / bitfield data)
    BitShuffle,
    /// Floating point precision truncation (zeros least-significant mantissa bits)
    TruncPrecision { prec_bits: i8 },
}

/// Composite filter pipeline: chains arbitrary filters in sequence
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FilterPipeline {
    /// Target element typesize in bytes (1, 2, 4, or 8)
    pub typesize: u8,
    /// Ordered list of filters to execute in sequence
    pub filters: Vec<FilterType>,
}

impl FilterPipeline {
    /// Create a new empty pipeline for the given typesize
    pub fn new(typesize: u8) -> Self {
        Self {
            typesize,
            filters: Vec::new(),
        }
    }

    /// Append a filter operation to the pipeline
    pub fn then(mut self, filter: FilterType) -> Self {
        self.filters.push(filter);
        self
    }

    /// Forward pipeline: apply filters in specified order
    pub fn apply(&self, data: &[u8]) -> Vec<u8> {
        let mut cur = data.to_vec();
        for f in &self.filters {
            match f {
                FilterType::Delta => delta_encode_inplace(&mut cur, self.typesize as usize),
                FilterType::ByteShuffle => cur = shuffle_encode(&cur, self.typesize as usize),
                FilterType::BitShuffle => {
                    if self.typesize > 0 {
                        if self.typesize > 1 {
                            cur = shuffle_encode(&cur, self.typesize as usize);
                        }
                        transpose_bit_blocks_inplace(&mut cur);
                    }
                }
                FilterType::TruncPrecision { prec_bits } => {
                    trunc_precision_encode_inplace(&mut cur, self.typesize as usize, *prec_bits);
                }
            }
        }
        cur
    }

    /// Reverse pipeline: apply inverse filters in reverse order
    pub fn unapply(&self, data: &[u8]) -> Vec<u8> {
        let mut cur = data.to_vec();
        self.unapply_in_place(&mut cur);
        cur
    }

    /// Reverse pipeline in-place: avoids an extra heap allocation when only
    /// in-place filters (such as `Delta` or `TruncPrecision`) are active.
    pub fn unapply_in_place(&self, data: &mut Vec<u8>) {
        for f in self.filters.iter().rev() {
            match f {
                FilterType::Delta => delta_decode_inplace(data, self.typesize as usize),
                FilterType::ByteShuffle => *data = shuffle_decode(data, self.typesize as usize),
                FilterType::BitShuffle => {
                    if self.typesize > 0 {
                        transpose_bit_blocks_inplace(data);
                        if self.typesize > 1 {
                            *data = shuffle_decode(data, self.typesize as usize);
                        }
                    }
                }
                FilterType::TruncPrecision { .. } => {} // Truncation is lossy; unapply is identity
            }
        }
    }
}

#[cfg(feature = "blosc2")]
/// Native C-Blosc2 compression wrapper for compressing a chunk with arbitrary C-Blosc2 filters and codecs
pub fn blosc2_compress(
    data: &[u8],
    typesize: usize,
    filters: &[blosc2::Filter],
    codec: blosc2::CompressAlgo,
    clevel: u32,
) -> Result<Vec<u8>, blosc2::Error> {
    let mut params = blosc2::CParams::default();
    params.typesize(typesize.clamp(1, 255))?;
    params.clevel(clevel);
    params.compressor(codec);
    params.filters(filters)?;
    let mut encoder = blosc2::chunk::Encoder::new(params)?;
    let chunk = encoder.compress(data)?;
    Ok(chunk.as_bytes().to_vec())
}

#[cfg(feature = "blosc2")]
/// Native C-Blosc2 decompression wrapper for decompressing a chunk
pub fn blosc2_decompress(compressed: &[u8]) -> Result<Vec<u8>, blosc2::Error> {
    let mut decoder = blosc2::chunk::Decoder::new(blosc2::DParams::default())?;
    let decompressed = decoder.decompress(compressed)?;
    Ok(decompressed.to_vec())
}

/// Configuration for pre-compression data filters (compact representation stored in Inode metadata).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FilterConfig {
    /// Element size in bytes for shuffle/delta (1, 2, 4, or 8). 0 = disabled.
    pub typesize: u8,
    /// Enable first-order delta encoding
    pub delta: bool,
    /// Enable byte shuffle
    pub shuffle: bool,
    /// Enable bit shuffle
    #[serde(default)]
    pub bitshuffle: bool,
}

impl FilterConfig {
    /// No filters applied
    pub fn none() -> Self {
        Self::default()
    }

    /// Standard numeric pipeline: Delta followed by Byte Shuffle
    pub fn numeric(typesize: u8) -> Self {
        Self {
            typesize,
            delta: true,
            shuffle: true,
            bitshuffle: false,
        }
    }

    /// Delta filter only (first-order difference)
    pub fn delta_only(typesize: u8) -> Self {
        Self {
            typesize,
            delta: true,
            shuffle: false,
            bitshuffle: false,
        }
    }

    /// Byte shuffle filter only (matrix transposition)
    pub fn shuffle_only(typesize: u8) -> Self {
        Self {
            typesize,
            delta: false,
            shuffle: true,
            bitshuffle: false,
        }
    }

    /// Bit shuffle filter only (bit-level matrix transposition)
    pub fn bitshuffle_only(typesize: u8) -> Self {
        Self {
            typesize,
            delta: false,
            shuffle: false,
            bitshuffle: true,
        }
    }

    /// Custom filter combination
    pub fn custom(typesize: u8, delta: bool, shuffle: bool) -> Self {
        Self {
            typesize,
            delta,
            shuffle,
            bitshuffle: false,
        }
    }

    /// Check if any filter is enabled
    pub fn is_active(&self) -> bool {
        self.typesize > 0 && (self.delta || self.shuffle || self.bitshuffle)
    }

    /// Convert to a composite FilterPipeline
    pub fn to_pipeline(&self) -> FilterPipeline {
        let mut p = FilterPipeline::new(self.typesize);
        if self.delta {
            p = p.then(FilterType::Delta);
        }
        if self.shuffle {
            p = p.then(FilterType::ByteShuffle);
        }
        if self.bitshuffle {
            p = p.then(FilterType::BitShuffle);
        }
        p
    }
}

#[cfg(all(not(kani), target_arch = "x86_64"))]
#[inline]
unsafe fn sse2_deinterleave_u16_bytes(
    a: std::arch::x86_64::__m128i,
    b: std::arch::x86_64::__m128i,
) -> (std::arch::x86_64::__m128i, std::arch::x86_64::__m128i) {
    use std::arch::x86_64::*;
    let low_mask = _mm_set1_epi16(0x00FFu16 as i16);
    let lo_a = _mm_and_si128(a, low_mask);
    let lo_b = _mm_and_si128(b, low_mask);
    let hi_a = _mm_srli_epi16::<8>(a);
    let hi_b = _mm_srli_epi16::<8>(b);
    (_mm_packus_epi16(lo_a, lo_b), _mm_packus_epi16(hi_a, hi_b))
}

/// In-place first-order delta encoding with zero allocations
#[inline]
pub fn delta_encode_inplace(data: &mut [u8], typesize: usize) {
    if typesize == 0 {
        return;
    }
    let n = data.len() / typesize;
    if n <= 1 {
        return;
    }

    #[cfg(target_endian = "little")]
    {
        // On Little-Endian architectures (x86_64, AArch64 / Apple Silicon), the in-memory
        // byte representation matches on-disk format. Because each lane in backwards-order
        // delta encoding `cur[i] -= cur[i-1]` depends only on original inputs, chunks from the
        // tail downward are 100% SIMD-parallel once the small remainder tail is handled.
        let ptr = data.as_mut_ptr();
        match typesize {
            1 => {
                #[allow(unused_mut)]
                let mut simd_end = 1usize;
                #[cfg(all(not(kani), target_arch = "aarch64"))]
                if n > 32 {
                    simd_end = 1 + ((n - 1) / 32) * 32;
                    for i in (simd_end..n).rev() {
                        data[i] = data[i].wrapping_sub(data[i - 1]);
                    }
                    unsafe {
                        use std::arch::aarch64::*;
                        let mut k = simd_end;
                        while k > 1 {
                            k -= 32;
                            let p = ptr.add(k);
                            let prev0 = vld1q_u8(p.sub(1));
                            let cur0 = vld1q_u8(p);
                            let prev1 = vld1q_u8(p.add(15));
                            let cur1 = vld1q_u8(p.add(16));
                            vst1q_u8(p, vsubq_u8(cur0, prev0));
                            vst1q_u8(p.add(16), vsubq_u8(cur1, prev1));
                        }
                    }
                    return;
                }
                #[cfg(all(not(kani), target_arch = "x86_64"))]
                if n > 32 {
                    simd_end = 1 + ((n - 1) / 32) * 32;
                    for i in (simd_end..n).rev() {
                        data[i] = data[i].wrapping_sub(data[i - 1]);
                    }
                    unsafe {
                        use std::arch::x86_64::*;
                        let mut k = simd_end;
                        while k > 1 {
                            k -= 32;
                            let p = ptr.add(k);
                            let prev0 = _mm_loadu_si128(p.sub(1) as *const __m128i);
                            let cur0 = _mm_loadu_si128(p as *const __m128i);
                            let prev1 = _mm_loadu_si128(p.add(15) as *const __m128i);
                            let cur1 = _mm_loadu_si128(p.add(16) as *const __m128i);
                            _mm_storeu_si128(p as *mut __m128i, _mm_sub_epi8(cur0, prev0));
                            _mm_storeu_si128(p.add(16) as *mut __m128i, _mm_sub_epi8(cur1, prev1));
                        }
                    }
                    return;
                }
                for i in (simd_end..n).rev() {
                    data[i] = data[i].wrapping_sub(data[i - 1]);
                }
                return;
            }
            2 if (ptr as usize).is_multiple_of(std::mem::align_of::<u16>()) => {
                let slice = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u16, n) };
                #[cfg(all(not(kani), target_arch = "aarch64"))]
                if n > 16 {
                    let simd_end = 1 + ((n - 1) / 16) * 16;
                    for i in (simd_end..n).rev() {
                        slice[i] = slice[i].wrapping_sub(slice[i - 1]);
                    }
                    unsafe {
                        use std::arch::aarch64::*;
                        let p_base = slice.as_mut_ptr();
                        let mut k = simd_end;
                        while k > 1 {
                            k -= 16;
                            let p = p_base.add(k);
                            let prev0 = vld1q_u16(p.sub(1));
                            let cur0 = vld1q_u16(p);
                            let prev1 = vld1q_u16(p.add(7));
                            let cur1 = vld1q_u16(p.add(8));
                            vst1q_u16(p, vsubq_u16(cur0, prev0));
                            vst1q_u16(p.add(8), vsubq_u16(cur1, prev1));
                        }
                    }
                    return;
                }
                #[cfg(all(not(kani), target_arch = "x86_64"))]
                if n > 16 {
                    let simd_end = 1 + ((n - 1) / 16) * 16;
                    for i in (simd_end..n).rev() {
                        slice[i] = slice[i].wrapping_sub(slice[i - 1]);
                    }
                    unsafe {
                        use std::arch::x86_64::*;
                        let p_base = slice.as_mut_ptr();
                        let mut k = simd_end;
                        while k > 1 {
                            k -= 16;
                            let p = p_base.add(k);
                            let prev0 = _mm_loadu_si128(p.sub(1) as *const __m128i);
                            let cur0 = _mm_loadu_si128(p as *const __m128i);
                            let prev1 = _mm_loadu_si128(p.add(7) as *const __m128i);
                            let cur1 = _mm_loadu_si128(p.add(8) as *const __m128i);
                            _mm_storeu_si128(p as *mut __m128i, _mm_sub_epi16(cur0, prev0));
                            _mm_storeu_si128(p.add(8) as *mut __m128i, _mm_sub_epi16(cur1, prev1));
                        }
                    }
                    return;
                }
                for i in (1..n).rev() {
                    slice[i] = slice[i].wrapping_sub(slice[i - 1]);
                }
                return;
            }
            4 if (ptr as usize).is_multiple_of(std::mem::align_of::<u32>()) => {
                let slice = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u32, n) };
                #[cfg(all(not(kani), target_arch = "aarch64"))]
                if n > 8 {
                    let simd_end = 1 + ((n - 1) / 8) * 8;
                    for i in (simd_end..n).rev() {
                        slice[i] = slice[i].wrapping_sub(slice[i - 1]);
                    }
                    unsafe {
                        use std::arch::aarch64::*;
                        let p_base = slice.as_mut_ptr();
                        let mut k = simd_end;
                        while k > 1 {
                            k -= 8;
                            let p = p_base.add(k);
                            let prev0 = vld1q_u32(p.sub(1));
                            let cur0 = vld1q_u32(p);
                            let prev1 = vld1q_u32(p.add(3));
                            let cur1 = vld1q_u32(p.add(4));
                            vst1q_u32(p, vsubq_u32(cur0, prev0));
                            vst1q_u32(p.add(4), vsubq_u32(cur1, prev1));
                        }
                    }
                    return;
                }
                #[cfg(all(not(kani), target_arch = "x86_64"))]
                if n > 8 {
                    let simd_end = 1 + ((n - 1) / 8) * 8;
                    for i in (simd_end..n).rev() {
                        slice[i] = slice[i].wrapping_sub(slice[i - 1]);
                    }
                    unsafe {
                        use std::arch::x86_64::*;
                        let p_base = slice.as_mut_ptr();
                        let mut k = simd_end;
                        while k > 1 {
                            k -= 8;
                            let p = p_base.add(k);
                            let prev0 = _mm_loadu_si128(p.sub(1) as *const __m128i);
                            let cur0 = _mm_loadu_si128(p as *const __m128i);
                            let prev1 = _mm_loadu_si128(p.add(3) as *const __m128i);
                            let cur1 = _mm_loadu_si128(p.add(4) as *const __m128i);
                            _mm_storeu_si128(p as *mut __m128i, _mm_sub_epi32(cur0, prev0));
                            _mm_storeu_si128(p.add(4) as *mut __m128i, _mm_sub_epi32(cur1, prev1));
                        }
                    }
                    return;
                }
                for i in (1..n).rev() {
                    slice[i] = slice[i].wrapping_sub(slice[i - 1]);
                }
                return;
            }
            8 if (ptr as usize).is_multiple_of(std::mem::align_of::<u64>()) => {
                let slice = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u64, n) };
                #[cfg(all(not(kani), target_arch = "aarch64"))]
                if n > 4 {
                    let simd_end = 1 + ((n - 1) / 4) * 4;
                    for i in (simd_end..n).rev() {
                        slice[i] = slice[i].wrapping_sub(slice[i - 1]);
                    }
                    unsafe {
                        use std::arch::aarch64::*;
                        let p_base = slice.as_mut_ptr();
                        let mut k = simd_end;
                        while k > 1 {
                            k -= 4;
                            let p = p_base.add(k);
                            let prev0 = vld1q_u64(p.sub(1));
                            let cur0 = vld1q_u64(p);
                            let prev1 = vld1q_u64(p.add(1));
                            let cur1 = vld1q_u64(p.add(2));
                            vst1q_u64(p, vsubq_u64(cur0, prev0));
                            vst1q_u64(p.add(2), vsubq_u64(cur1, prev1));
                        }
                    }
                    return;
                }
                #[cfg(all(not(kani), target_arch = "x86_64"))]
                if n > 4 {
                    let simd_end = 1 + ((n - 1) / 4) * 4;
                    for i in (simd_end..n).rev() {
                        slice[i] = slice[i].wrapping_sub(slice[i - 1]);
                    }
                    unsafe {
                        use std::arch::x86_64::*;
                        let p_base = slice.as_mut_ptr();
                        let mut k = simd_end;
                        while k > 1 {
                            k -= 4;
                            let p = p_base.add(k);
                            let prev0 = _mm_loadu_si128(p.sub(1) as *const __m128i);
                            let cur0 = _mm_loadu_si128(p as *const __m128i);
                            let prev1 = _mm_loadu_si128(p.add(1) as *const __m128i);
                            let cur1 = _mm_loadu_si128(p.add(2) as *const __m128i);
                            _mm_storeu_si128(p as *mut __m128i, _mm_sub_epi64(cur0, prev0));
                            _mm_storeu_si128(p.add(2) as *mut __m128i, _mm_sub_epi64(cur1, prev1));
                        }
                    }
                    return;
                }
                for i in (1..n).rev() {
                    slice[i] = slice[i].wrapping_sub(slice[i - 1]);
                }
                return;
            }
            _ => {}
        }
    }

    // Portable / unaligned / Big-Endian fallback:
    // Explicitly uses from_le_bytes and to_le_bytes to guarantee uniform on-disk byte format.
    match typesize {
        1 => {
            for i in (1..n).rev() {
                data[i] = data[i].wrapping_sub(data[i - 1]);
            }
        }
        2 => {
            for i in (1..n).rev() {
                let curr = u16::from_le_bytes(data[i * 2..i * 2 + 2].try_into().unwrap());
                let prev = u16::from_le_bytes(data[(i - 1) * 2..i * 2].try_into().unwrap());
                let diff = curr.wrapping_sub(prev).to_le_bytes();
                data[i * 2..i * 2 + 2].copy_from_slice(&diff);
            }
        }
        4 => {
            for i in (1..n).rev() {
                let curr = u32::from_le_bytes(data[i * 4..i * 4 + 4].try_into().unwrap());
                let prev = u32::from_le_bytes(data[(i - 1) * 4..i * 4].try_into().unwrap());
                let diff = curr.wrapping_sub(prev).to_le_bytes();
                data[i * 4..i * 4 + 4].copy_from_slice(&diff);
            }
        }
        8 => {
            for i in (1..n).rev() {
                let curr = u64::from_le_bytes(data[i * 8..i * 8 + 8].try_into().unwrap());
                let prev = u64::from_le_bytes(data[(i - 1) * 8..i * 8].try_into().unwrap());
                let diff = curr.wrapping_sub(prev).to_le_bytes();
                data[i * 8..i * 8 + 8].copy_from_slice(&diff);
            }
        }
        _ => {}
    }
}

/// In-place first-order delta decoding with zero allocations
#[inline]
pub fn delta_decode_inplace(data: &mut [u8], typesize: usize) {
    if typesize == 0 {
        return;
    }
    let n = data.len() / typesize;
    if n <= 1 {
        return;
    }

    #[cfg(all(not(kani), any(target_arch = "aarch64", target_arch = "x86_64")))]
    macro_rules! delta_decode_tree_scan {
        ($ptr:expr, $n:expr, $i:ident, $lanes:expr, $vzero:expr, $vload:expr, $vstore:expr, $vadd:expr, $vscan16:expr, $vbcast_last:expr) => {{
            let p_base = $ptr;
            let mut carry = $vzero;
            $i = 0;
            let stride_8 = 8 * $lanes;
            while $i + stride_8 <= $n {
                let p = p_base.add($i);
                let w0 = $vscan16($vload(p));
                let s1 = $vscan16($vload(p.add($lanes)));
                let mut w2 = $vscan16($vload(p.add(2 * $lanes)));
                let s3 = $vscan16($vload(p.add(3 * $lanes)));
                let s4 = $vscan16($vload(p.add(4 * $lanes)));
                let s5 = $vscan16($vload(p.add(5 * $lanes)));
                let s6 = $vscan16($vload(p.add(6 * $lanes)));
                let s7 = $vscan16($vload(p.add(7 * $lanes)));

                // Level 1: combine odd-even 16B pairs independently of `carry`.
                let w1 = $vadd(s1, $vbcast_last(w0));
                let w3_base = $vadd(s3, $vbcast_last(w2));
                let w5_base = $vadd(s5, $vbcast_last(s4));
                let u7 = $vadd(s7, $vbcast_last(s6));

                // Level 2: combine 32B halves into two independent 64B scans.
                let c01 = $vbcast_last(w1);
                w2 = $vadd(w2, c01);
                let w3 = $vadd(w3_base, c01);

                let c45 = $vbcast_last(w5_base);
                let w6_base = $vadd(s6, c45);
                let w7_base = $vadd(u7, c45);

                // Level 3: fold [0..64B) total into [64..128B) and precompute 128B block_total.
                let c03 = $vbcast_last(w3);
                let w4 = $vadd(s4, c03);
                let w5 = $vadd(w5_base, c03);
                let w6 = $vadd(w6_base, c03);
                let w7 = $vadd(w7_base, c03);
                let block_total = $vbcast_last(w7);

                // Loop-carried critical path: ONLY 1 vector add (`carry += block_total`) per 128B!
                $vstore(p, $vadd(w0, carry));
                $vstore(p.add($lanes), $vadd(w1, carry));
                $vstore(p.add(2 * $lanes), $vadd(w2, carry));
                $vstore(p.add(3 * $lanes), $vadd(w3, carry));
                $vstore(p.add(4 * $lanes), $vadd(w4, carry));
                $vstore(p.add(5 * $lanes), $vadd(w5, carry));
                $vstore(p.add(6 * $lanes), $vadd(w6, carry));
                $vstore(p.add(7 * $lanes), $vadd(w7, carry));
                carry = $vadd(carry, block_total);
                $i += stride_8;
            }
            let stride_2 = 2 * $lanes;
            while $i + stride_2 <= $n {
                let p = p_base.add($i);
                let s0 = $vscan16($vload(p));
                let s1 = $vscan16($vload(p.add($lanes)));
                let s1_full = $vadd(s1, $vbcast_last(s0));
                let tail_total = $vbcast_last(s1_full);
                $vstore(p, $vadd(s0, carry));
                $vstore(p.add($lanes), $vadd(s1_full, carry));
                carry = $vadd(carry, tail_total);
                $i += stride_2;
            }
        }};
    }

    #[cfg(target_endian = "little")]
    {
        let ptr = data.as_mut_ptr();
        match typesize {
            1 => {
                #[allow(unused_mut)]
                let mut i = 1usize;
                #[cfg(all(not(kani), target_arch = "aarch64"))]
                if n >= 32 {
                    unsafe {
                        use std::arch::aarch64::*;
                        let zero = vdupq_n_u8(0);
                        delta_decode_tree_scan!(
                            ptr,
                            n,
                            i,
                            16,
                            zero,
                            |p| vld1q_u8(p),
                            |p, v| vst1q_u8(p, v),
                            |a, b| vaddq_u8(a, b),
                            |v| {
                                let s1 = vaddq_u8(v, vextq_u8::<15>(zero, v));
                                let s2 = vaddq_u8(s1, vextq_u8::<14>(zero, s1));
                                let s3 = vaddq_u8(s2, vextq_u8::<12>(zero, s2));
                                vaddq_u8(s3, vextq_u8::<8>(zero, s3))
                            },
                            |v| vdupq_laneq_u8::<15>(v)
                        );
                    }
                }
                #[cfg(all(not(kani), target_arch = "x86_64"))]
                if n >= 32 {
                    unsafe {
                        use std::arch::x86_64::*;
                        delta_decode_tree_scan!(
                            ptr,
                            n,
                            i,
                            16,
                            _mm_setzero_si128(),
                            |p: *mut u8| _mm_loadu_si128(p as *const __m128i),
                            |p: *mut u8, v| _mm_storeu_si128(p as *mut __m128i, v),
                            |a, b| _mm_add_epi8(a, b),
                            |v| {
                                let s1 = _mm_add_epi8(v, _mm_slli_si128::<1>(v));
                                let s2 = _mm_add_epi8(s1, _mm_slli_si128::<2>(s1));
                                let s3 = _mm_add_epi8(s2, _mm_slli_si128::<4>(s2));
                                _mm_add_epi8(s3, _mm_slli_si128::<8>(s3))
                            },
                            |v| {
                                let hi = _mm_unpackhi_epi8(v, v);
                                _mm_shuffle_epi32::<0xFF>(_mm_shufflehi_epi16::<0xFF>(hi))
                            }
                        );
                    }
                }
                for j in i.max(1)..n {
                    data[j] = data[j - 1].wrapping_add(data[j]);
                }
                return;
            }
            2 if (ptr as usize).is_multiple_of(std::mem::align_of::<u16>()) => {
                let slice = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u16, n) };
                #[allow(unused_mut)]
                let mut i = 1usize;
                #[cfg(all(not(kani), target_arch = "aarch64"))]
                if n >= 16 {
                    unsafe {
                        use std::arch::aarch64::*;
                        let zero = vdupq_n_u16(0);
                        delta_decode_tree_scan!(
                            slice.as_mut_ptr(),
                            n,
                            i,
                            8,
                            zero,
                            |p| vld1q_u16(p),
                            |p, v| vst1q_u16(p, v),
                            |a, b| vaddq_u16(a, b),
                            |v| {
                                let s1 = vaddq_u16(v, vextq_u16::<7>(zero, v));
                                let s2 = vaddq_u16(s1, vextq_u16::<6>(zero, s1));
                                vaddq_u16(s2, vextq_u16::<4>(zero, s2))
                            },
                            |v| vdupq_laneq_u16::<7>(v)
                        );
                    }
                }
                #[cfg(all(not(kani), target_arch = "x86_64"))]
                if n >= 16 {
                    unsafe {
                        use std::arch::x86_64::*;
                        delta_decode_tree_scan!(
                            slice.as_mut_ptr(),
                            n,
                            i,
                            8,
                            _mm_setzero_si128(),
                            |p: *mut u16| _mm_loadu_si128(p as *const __m128i),
                            |p: *mut u16, v| _mm_storeu_si128(p as *mut __m128i, v),
                            |a, b| _mm_add_epi16(a, b),
                            |v| {
                                let s1 = _mm_add_epi16(v, _mm_slli_si128::<2>(v));
                                let s2 = _mm_add_epi16(s1, _mm_slli_si128::<4>(s1));
                                _mm_add_epi16(s2, _mm_slli_si128::<8>(s2))
                            },
                            |v| _mm_shuffle_epi32::<0xFF>(_mm_shufflehi_epi16::<0xFF>(v))
                        );
                    }
                }
                for j in i.max(1)..n {
                    slice[j] = slice[j - 1].wrapping_add(slice[j]);
                }
                return;
            }
            4 if (ptr as usize).is_multiple_of(std::mem::align_of::<u32>()) => {
                let slice = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u32, n) };
                #[allow(unused_mut)]
                let mut i = 1usize;
                #[cfg(all(not(kani), target_arch = "aarch64"))]
                if n >= 8 {
                    unsafe {
                        use std::arch::aarch64::*;
                        let zero = vdupq_n_u32(0);
                        delta_decode_tree_scan!(
                            slice.as_mut_ptr(),
                            n,
                            i,
                            4,
                            zero,
                            |p| vld1q_u32(p),
                            |p, v| vst1q_u32(p, v),
                            |a, b| vaddq_u32(a, b),
                            |v| {
                                let s1 = vaddq_u32(v, vextq_u32::<3>(zero, v));
                                vaddq_u32(s1, vextq_u32::<2>(zero, s1))
                            },
                            |v| vdupq_laneq_u32::<3>(v)
                        );
                    }
                }
                #[cfg(all(not(kani), target_arch = "x86_64"))]
                if n >= 8 {
                    unsafe {
                        use std::arch::x86_64::*;
                        delta_decode_tree_scan!(
                            slice.as_mut_ptr(),
                            n,
                            i,
                            4,
                            _mm_setzero_si128(),
                            |p: *mut u32| _mm_loadu_si128(p as *const __m128i),
                            |p: *mut u32, v| _mm_storeu_si128(p as *mut __m128i, v),
                            |a, b| _mm_add_epi32(a, b),
                            |v| {
                                let s1 = _mm_add_epi32(v, _mm_slli_si128::<4>(v));
                                _mm_add_epi32(s1, _mm_slli_si128::<8>(s1))
                            },
                            |v| _mm_shuffle_epi32::<0xFF>(v)
                        );
                    }
                }
                for j in i.max(1)..n {
                    slice[j] = slice[j - 1].wrapping_add(slice[j]);
                }
                return;
            }
            8 if (ptr as usize).is_multiple_of(std::mem::align_of::<u64>()) => {
                let slice = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u64, n) };
                #[allow(unused_mut)]
                let mut i = 1usize;
                #[cfg(all(not(kani), target_arch = "aarch64"))]
                if n >= 4 {
                    unsafe {
                        use std::arch::aarch64::*;
                        let zero = vdupq_n_u64(0);
                        delta_decode_tree_scan!(
                            slice.as_mut_ptr(),
                            n,
                            i,
                            2,
                            zero,
                            |p| vld1q_u64(p),
                            |p, v| vst1q_u64(p, v),
                            |a, b| vaddq_u64(a, b),
                            |v| vaddq_u64(v, vextq_u64::<1>(zero, v)),
                            |v| vdupq_laneq_u64::<1>(v)
                        );
                    }
                }
                #[cfg(all(not(kani), target_arch = "x86_64"))]
                if n >= 4 {
                    unsafe {
                        use std::arch::x86_64::*;
                        delta_decode_tree_scan!(
                            slice.as_mut_ptr(),
                            n,
                            i,
                            2,
                            _mm_setzero_si128(),
                            |p: *mut u64| _mm_loadu_si128(p as *const __m128i),
                            |p: *mut u64, v| _mm_storeu_si128(p as *mut __m128i, v),
                            |a, b| _mm_add_epi64(a, b),
                            |v| _mm_add_epi64(v, _mm_slli_si128::<8>(v)),
                            |v| _mm_shuffle_epi32::<0xEE>(v)
                        );
                    }
                }
                for j in i.max(1)..n {
                    slice[j] = slice[j - 1].wrapping_add(slice[j]);
                }
                return;
            }
            _ => {}
        }
    }

    // Portable / unaligned / Big-Endian fallback:
    match typesize {
        1 => {
            for i in 1..n {
                data[i] = data[i - 1].wrapping_add(data[i]);
            }
        }
        2 => {
            for i in 1..n {
                let prev = u16::from_le_bytes(data[(i - 1) * 2..i * 2].try_into().unwrap());
                let curr = u16::from_le_bytes(data[i * 2..i * 2 + 2].try_into().unwrap());
                let sum = prev.wrapping_add(curr).to_le_bytes();
                data[i * 2..i * 2 + 2].copy_from_slice(&sum);
            }
        }
        4 => {
            for i in 1..n {
                let prev = u32::from_le_bytes(data[(i - 1) * 4..i * 4].try_into().unwrap());
                let curr = u32::from_le_bytes(data[i * 4..i * 4 + 4].try_into().unwrap());
                let sum = prev.wrapping_add(curr).to_le_bytes();
                data[i * 4..i * 4 + 4].copy_from_slice(&sum);
            }
        }
        8 => {
            for i in 1..n {
                let prev = u64::from_le_bytes(data[(i - 1) * 8..i * 8].try_into().unwrap());
                let curr = u64::from_le_bytes(data[i * 8..i * 8 + 8].try_into().unwrap());
                let sum = prev.wrapping_add(curr).to_le_bytes();
                data[i * 8..i * 8 + 8].copy_from_slice(&sum);
            }
        }
        _ => {}
    }
}

pub fn delta_encode(data: &[u8], typesize: usize) -> Vec<u8> {
    let mut out = data.to_vec();
    delta_encode_inplace(&mut out, typesize);
    out
}

pub fn delta_decode(data: &[u8], typesize: usize) -> Vec<u8> {
    let mut out = data.to_vec();
    delta_decode_inplace(&mut out, typesize);
    out
}

pub fn shuffle_encode(data: &[u8], typesize: usize) -> Vec<u8> {
    if typesize <= 1 || data.is_empty() {
        return data.to_vec();
    }
    let len = data.len();
    let n = len / typesize;

    #[cfg(not(kani))]
    let mut out: Vec<u8> = Vec::with_capacity(len);
    #[cfg(kani)]
    let mut out: Vec<u8> = vec![0; len];

    let src = data.as_ptr();
    let dst: *mut u8 = out.as_mut_ptr();

    match typesize {
        2 => {
            let mut i = 0usize;
            #[cfg(all(not(kani), target_arch = "aarch64"))]
            unsafe {
                use std::arch::aarch64::*;
                let d0 = dst;
                let d1 = dst.add(n);
                while i + 16 <= n {
                    let pair = vld2q_u8(src.add(i * 2));
                    vst1q_u8(d0.add(i), pair.0);
                    vst1q_u8(d1.add(i), pair.1);
                    i += 16;
                }
            }
            #[cfg(all(not(kani), target_arch = "x86_64"))]
            unsafe {
                use std::arch::x86_64::*;
                let d0 = dst;
                let d1 = dst.add(n);
                while i + 16 <= n {
                    let v0 = _mm_loadu_si128(src.add(i * 2) as *const __m128i);
                    let v1 = _mm_loadu_si128(src.add(i * 2 + 16) as *const __m128i);
                    let (s0, s1) = sse2_deinterleave_u16_bytes(v0, v1);
                    _mm_storeu_si128(d0.add(i) as *mut __m128i, s0);
                    _mm_storeu_si128(d1.add(i) as *mut __m128i, s1);
                    i += 16;
                }
            }
            while i < n {
                unsafe {
                    *dst.add(i) = *src.add(i * 2);
                    *dst.add(n + i) = *src.add(i * 2 + 1);
                }
                i += 1;
            }
        }
        4 => {
            let mut i = 0usize;
            #[cfg(all(not(kani), target_arch = "aarch64"))]
            unsafe {
                use std::arch::aarch64::*;
                let d0 = dst;
                let d1 = dst.add(n);
                let d2 = dst.add(2 * n);
                let d3 = dst.add(3 * n);
                while i + 16 <= n {
                    let quad = vld4q_u8(src.add(i * 4));
                    vst1q_u8(d0.add(i), quad.0);
                    vst1q_u8(d1.add(i), quad.1);
                    vst1q_u8(d2.add(i), quad.2);
                    vst1q_u8(d3.add(i), quad.3);
                    i += 16;
                }
            }
            #[cfg(all(not(kani), target_arch = "x86_64"))]
            unsafe {
                use std::arch::x86_64::*;
                let d0 = dst;
                let d1 = dst.add(n);
                let d2 = dst.add(2 * n);
                let d3 = dst.add(3 * n);
                while i + 16 <= n {
                    let p = src.add(i * 4) as *const __m128i;
                    let v0 = _mm_loadu_si128(p);
                    let v1 = _mm_loadu_si128(p.add(1));
                    let v2 = _mm_loadu_si128(p.add(2));
                    let v3 = _mm_loadu_si128(p.add(3));
                    let (e01, o01) = sse2_deinterleave_u16_bytes(v0, v1);
                    let (e23, o23) = sse2_deinterleave_u16_bytes(v2, v3);
                    let (s0, s2) = sse2_deinterleave_u16_bytes(e01, e23);
                    let (s1, s3) = sse2_deinterleave_u16_bytes(o01, o23);
                    _mm_storeu_si128(d0.add(i) as *mut __m128i, s0);
                    _mm_storeu_si128(d1.add(i) as *mut __m128i, s1);
                    _mm_storeu_si128(d2.add(i) as *mut __m128i, s2);
                    _mm_storeu_si128(d3.add(i) as *mut __m128i, s3);
                    i += 16;
                }
            }
            while i < n {
                unsafe {
                    *dst.add(i) = *src.add(i * 4);
                    *dst.add(n + i) = *src.add(i * 4 + 1);
                    *dst.add(2 * n + i) = *src.add(i * 4 + 2);
                    *dst.add(3 * n + i) = *src.add(i * 4 + 3);
                }
                i += 1;
            }
        }
        8 => {
            let mut i = 0usize;
            #[cfg(all(not(kani), target_arch = "aarch64"))]
            unsafe {
                use std::arch::aarch64::*;
                let d0 = dst;
                let d1 = dst.add(n);
                let d2 = dst.add(2 * n);
                let d3 = dst.add(3 * n);
                let d4 = dst.add(4 * n);
                let d5 = dst.add(5 * n);
                let d6 = dst.add(6 * n);
                let d7 = dst.add(7 * n);
                while i + 16 <= n {
                    let a = vld4q_u8(src.add(i * 8));
                    let b = vld4q_u8(src.add(i * 8 + 64));
                    vst1q_u8(d0.add(i), vuzp1q_u8(a.0, b.0));
                    vst1q_u8(d4.add(i), vuzp2q_u8(a.0, b.0));
                    vst1q_u8(d1.add(i), vuzp1q_u8(a.1, b.1));
                    vst1q_u8(d5.add(i), vuzp2q_u8(a.1, b.1));
                    vst1q_u8(d2.add(i), vuzp1q_u8(a.2, b.2));
                    vst1q_u8(d6.add(i), vuzp2q_u8(a.2, b.2));
                    vst1q_u8(d3.add(i), vuzp1q_u8(a.3, b.3));
                    vst1q_u8(d7.add(i), vuzp2q_u8(a.3, b.3));
                    i += 16;
                }
            }
            #[cfg(all(not(kani), target_arch = "x86_64"))]
            unsafe {
                use std::arch::x86_64::*;
                let d0 = dst;
                let d1 = dst.add(n);
                let d2 = dst.add(2 * n);
                let d3 = dst.add(3 * n);
                let d4 = dst.add(4 * n);
                let d5 = dst.add(5 * n);
                let d6 = dst.add(6 * n);
                let d7 = dst.add(7 * n);
                while i + 16 <= n {
                    let p = src.add(i * 8) as *const __m128i;
                    let (e01, o01) =
                        sse2_deinterleave_u16_bytes(_mm_loadu_si128(p), _mm_loadu_si128(p.add(1)));
                    let (e23, o23) = sse2_deinterleave_u16_bytes(
                        _mm_loadu_si128(p.add(2)),
                        _mm_loadu_si128(p.add(3)),
                    );
                    let (e45, o45) = sse2_deinterleave_u16_bytes(
                        _mm_loadu_si128(p.add(4)),
                        _mm_loadu_si128(p.add(5)),
                    );
                    let (e67, o67) = sse2_deinterleave_u16_bytes(
                        _mm_loadu_si128(p.add(6)),
                        _mm_loadu_si128(p.add(7)),
                    );
                    let (ee0, eo0) = sse2_deinterleave_u16_bytes(e01, e23);
                    let (ee1, eo1) = sse2_deinterleave_u16_bytes(e45, e67);
                    let (oe0, oo0) = sse2_deinterleave_u16_bytes(o01, o23);
                    let (oe1, oo1) = sse2_deinterleave_u16_bytes(o45, o67);
                    let (s0, s4) = sse2_deinterleave_u16_bytes(ee0, ee1);
                    let (s2, s6) = sse2_deinterleave_u16_bytes(eo0, eo1);
                    let (s1, s5) = sse2_deinterleave_u16_bytes(oe0, oe1);
                    let (s3, s7) = sse2_deinterleave_u16_bytes(oo0, oo1);
                    _mm_storeu_si128(d0.add(i) as *mut __m128i, s0);
                    _mm_storeu_si128(d1.add(i) as *mut __m128i, s1);
                    _mm_storeu_si128(d2.add(i) as *mut __m128i, s2);
                    _mm_storeu_si128(d3.add(i) as *mut __m128i, s3);
                    _mm_storeu_si128(d4.add(i) as *mut __m128i, s4);
                    _mm_storeu_si128(d5.add(i) as *mut __m128i, s5);
                    _mm_storeu_si128(d6.add(i) as *mut __m128i, s6);
                    _mm_storeu_si128(d7.add(i) as *mut __m128i, s7);
                    i += 16;
                }
            }
            while i < n {
                unsafe {
                    *dst.add(i) = *src.add(i * 8);
                    *dst.add(n + i) = *src.add(i * 8 + 1);
                    *dst.add(2 * n + i) = *src.add(i * 8 + 2);
                    *dst.add(3 * n + i) = *src.add(i * 8 + 3);
                    *dst.add(4 * n + i) = *src.add(i * 8 + 4);
                    *dst.add(5 * n + i) = *src.add(i * 8 + 5);
                    *dst.add(6 * n + i) = *src.add(i * 8 + 6);
                    *dst.add(7 * n + i) = *src.add(i * 8 + 7);
                }
                i += 1;
            }
        }
        _ => {
            for i in 0..n {
                for j in 0..typesize {
                    unsafe {
                        *dst.add(j * n + i) = *src.add(i * typesize + j);
                    }
                }
            }
        }
    }

    let tail_start = n * typesize;
    if tail_start < len {
        // SAFETY: `tail_start < len`, `src` and `dst` are non-overlapping, and `dst` has capacity `len`.
        unsafe {
            std::ptr::copy_nonoverlapping(
                src.add(tail_start),
                dst.add(tail_start),
                len - tail_start,
            );
        }
    }

    // SAFETY: Every byte in `0..len` (`typesize * n` stream bytes plus `tail_start..len`
    // remainder) has now been initialized via `dst`.
    #[cfg(not(kani))]
    unsafe {
        out.set_len(len);
    }

    out
}

pub fn shuffle_decode(data: &[u8], typesize: usize) -> Vec<u8> {
    if typesize <= 1 || data.is_empty() {
        return data.to_vec();
    }
    let len = data.len();
    let n = len / typesize;

    #[cfg(not(kani))]
    let mut out: Vec<u8> = Vec::with_capacity(len);
    #[cfg(kani)]
    let mut out: Vec<u8> = vec![0; len];

    let src = data.as_ptr();
    let dst: *mut u8 = out.as_mut_ptr();

    match typesize {
        2 => {
            let mut i = 0usize;
            #[cfg(all(not(kani), target_arch = "aarch64"))]
            unsafe {
                use std::arch::aarch64::*;
                let s0 = src;
                let s1 = src.add(n);
                while i + 16 <= n {
                    let v0 = vld1q_u8(s0.add(i));
                    let v1 = vld1q_u8(s1.add(i));
                    vst2q_u8(dst.add(i * 2), uint8x16x2_t(v0, v1));
                    i += 16;
                }
            }
            #[cfg(all(not(kani), target_arch = "x86_64"))]
            unsafe {
                use std::arch::x86_64::*;
                let s0 = src;
                let s1 = src.add(n);
                while i + 16 <= n {
                    let v0 = _mm_loadu_si128(s0.add(i) as *const __m128i);
                    let v1 = _mm_loadu_si128(s1.add(i) as *const __m128i);
                    _mm_storeu_si128(dst.add(i * 2) as *mut __m128i, _mm_unpacklo_epi8(v0, v1));
                    _mm_storeu_si128(
                        dst.add(i * 2 + 16) as *mut __m128i,
                        _mm_unpackhi_epi8(v0, v1),
                    );
                    i += 16;
                }
            }
            while i < n {
                unsafe {
                    *dst.add(i * 2) = *src.add(i);
                    *dst.add(i * 2 + 1) = *src.add(n + i);
                }
                i += 1;
            }
        }
        4 => {
            let mut i = 0usize;
            #[cfg(all(not(kani), target_arch = "aarch64"))]
            unsafe {
                use std::arch::aarch64::*;
                let s0 = src;
                let s1 = src.add(n);
                let s2 = src.add(2 * n);
                let s3 = src.add(3 * n);
                while i + 16 <= n {
                    let v0 = vld1q_u8(s0.add(i));
                    let v1 = vld1q_u8(s1.add(i));
                    let v2 = vld1q_u8(s2.add(i));
                    let v3 = vld1q_u8(s3.add(i));
                    vst4q_u8(dst.add(i * 4), uint8x16x4_t(v0, v1, v2, v3));
                    i += 16;
                }
            }
            #[cfg(all(not(kani), target_arch = "x86_64"))]
            unsafe {
                use std::arch::x86_64::*;
                let s0 = src;
                let s1 = src.add(n);
                let s2 = src.add(2 * n);
                let s3 = src.add(3 * n);
                while i + 16 <= n {
                    let v0 = _mm_loadu_si128(s0.add(i) as *const __m128i);
                    let v1 = _mm_loadu_si128(s1.add(i) as *const __m128i);
                    let v2 = _mm_loadu_si128(s2.add(i) as *const __m128i);
                    let v3 = _mm_loadu_si128(s3.add(i) as *const __m128i);
                    let t01_lo = _mm_unpacklo_epi8(v0, v1);
                    let t01_hi = _mm_unpackhi_epi8(v0, v1);
                    let t23_lo = _mm_unpacklo_epi8(v2, v3);
                    let t23_hi = _mm_unpackhi_epi8(v2, v3);
                    let p = dst.add(i * 4) as *mut __m128i;
                    _mm_storeu_si128(p, _mm_unpacklo_epi16(t01_lo, t23_lo));
                    _mm_storeu_si128(p.add(1), _mm_unpackhi_epi16(t01_lo, t23_lo));
                    _mm_storeu_si128(p.add(2), _mm_unpacklo_epi16(t01_hi, t23_hi));
                    _mm_storeu_si128(p.add(3), _mm_unpackhi_epi16(t01_hi, t23_hi));
                    i += 16;
                }
            }
            while i < n {
                unsafe {
                    *dst.add(i * 4) = *src.add(i);
                    *dst.add(i * 4 + 1) = *src.add(n + i);
                    *dst.add(i * 4 + 2) = *src.add(2 * n + i);
                    *dst.add(i * 4 + 3) = *src.add(3 * n + i);
                }
                i += 1;
            }
        }
        8 => {
            let mut i = 0usize;
            #[cfg(all(not(kani), target_arch = "aarch64"))]
            unsafe {
                use std::arch::aarch64::*;
                let b0 = src;
                let b1 = src.add(n);
                let b2 = src.add(2 * n);
                let b3 = src.add(3 * n);
                let b4 = src.add(4 * n);
                let b5 = src.add(5 * n);
                let b6 = src.add(6 * n);
                let b7 = src.add(7 * n);
                while i + 16 <= n {
                    let s0 = vld1q_u8(b0.add(i));
                    let s1 = vld1q_u8(b1.add(i));
                    let s2 = vld1q_u8(b2.add(i));
                    let s3 = vld1q_u8(b3.add(i));
                    let s4 = vld1q_u8(b4.add(i));
                    let s5 = vld1q_u8(b5.add(i));
                    let s6 = vld1q_u8(b6.add(i));
                    let s7 = vld1q_u8(b7.add(i));
                    let a = uint8x16x4_t(
                        vzip1q_u8(s0, s4),
                        vzip1q_u8(s1, s5),
                        vzip1q_u8(s2, s6),
                        vzip1q_u8(s3, s7),
                    );
                    let b = uint8x16x4_t(
                        vzip2q_u8(s0, s4),
                        vzip2q_u8(s1, s5),
                        vzip2q_u8(s2, s6),
                        vzip2q_u8(s3, s7),
                    );
                    vst4q_u8(dst.add(i * 8), a);
                    vst4q_u8(dst.add(i * 8 + 64), b);
                    i += 16;
                }
            }
            #[cfg(all(not(kani), target_arch = "x86_64"))]
            unsafe {
                use std::arch::x86_64::*;
                let b0 = src;
                let b1 = src.add(n);
                let b2 = src.add(2 * n);
                let b3 = src.add(3 * n);
                let b4 = src.add(4 * n);
                let b5 = src.add(5 * n);
                let b6 = src.add(6 * n);
                let b7 = src.add(7 * n);
                while i + 16 <= n {
                    let s0 = _mm_loadu_si128(b0.add(i) as *const __m128i);
                    let s1 = _mm_loadu_si128(b1.add(i) as *const __m128i);
                    let s2 = _mm_loadu_si128(b2.add(i) as *const __m128i);
                    let s3 = _mm_loadu_si128(b3.add(i) as *const __m128i);
                    let s4 = _mm_loadu_si128(b4.add(i) as *const __m128i);
                    let s5 = _mm_loadu_si128(b5.add(i) as *const __m128i);
                    let s6 = _mm_loadu_si128(b6.add(i) as *const __m128i);
                    let s7 = _mm_loadu_si128(b7.add(i) as *const __m128i);

                    let t01_l = _mm_unpacklo_epi8(s0, s1);
                    let t01_h = _mm_unpackhi_epi8(s0, s1);
                    let t23_l = _mm_unpacklo_epi8(s2, s3);
                    let t23_h = _mm_unpackhi_epi8(s2, s3);
                    let t45_l = _mm_unpacklo_epi8(s4, s5);
                    let t45_h = _mm_unpackhi_epi8(s4, s5);
                    let t67_l = _mm_unpacklo_epi8(s6, s7);
                    let t67_h = _mm_unpackhi_epi8(s6, s7);

                    let q0 = _mm_unpacklo_epi16(t01_l, t23_l);
                    let q1 = _mm_unpackhi_epi16(t01_l, t23_l);
                    let q2 = _mm_unpacklo_epi16(t01_h, t23_h);
                    let q3 = _mm_unpackhi_epi16(t01_h, t23_h);
                    let q4 = _mm_unpacklo_epi16(t45_l, t67_l);
                    let q5 = _mm_unpackhi_epi16(t45_l, t67_l);
                    let q6 = _mm_unpacklo_epi16(t45_h, t67_h);
                    let q7 = _mm_unpackhi_epi16(t45_h, t67_h);

                    let p = dst.add(i * 8) as *mut __m128i;
                    _mm_storeu_si128(p, _mm_unpacklo_epi32(q0, q4));
                    _mm_storeu_si128(p.add(1), _mm_unpackhi_epi32(q0, q4));
                    _mm_storeu_si128(p.add(2), _mm_unpacklo_epi32(q1, q5));
                    _mm_storeu_si128(p.add(3), _mm_unpackhi_epi32(q1, q5));
                    _mm_storeu_si128(p.add(4), _mm_unpacklo_epi32(q2, q6));
                    _mm_storeu_si128(p.add(5), _mm_unpackhi_epi32(q2, q6));
                    _mm_storeu_si128(p.add(6), _mm_unpacklo_epi32(q3, q7));
                    _mm_storeu_si128(p.add(7), _mm_unpackhi_epi32(q3, q7));
                    i += 16;
                }
            }
            while i < n {
                unsafe {
                    *dst.add(i * 8) = *src.add(i);
                    *dst.add(i * 8 + 1) = *src.add(n + i);
                    *dst.add(i * 8 + 2) = *src.add(2 * n + i);
                    *dst.add(i * 8 + 3) = *src.add(3 * n + i);
                    *dst.add(i * 8 + 4) = *src.add(4 * n + i);
                    *dst.add(i * 8 + 5) = *src.add(5 * n + i);
                    *dst.add(i * 8 + 6) = *src.add(6 * n + i);
                    *dst.add(i * 8 + 7) = *src.add(7 * n + i);
                }
                i += 1;
            }
        }
        _ => {
            for i in 0..n {
                for j in 0..typesize {
                    unsafe {
                        *dst.add(i * typesize + j) = *src.add(j * n + i);
                    }
                }
            }
        }
    }

    let tail_start = n * typesize;
    if tail_start < len {
        // SAFETY: `tail_start < len`, `src` and `dst` are non-overlapping, and `dst` has capacity `len`.
        unsafe {
            std::ptr::copy_nonoverlapping(
                src.add(tail_start),
                dst.add(tail_start),
                len - tail_start,
            );
        }
    }

    // SAFETY: Every byte in `0..len` (`typesize * n` element bytes plus `tail_start..len`
    // remainder) has now been initialized via `dst`.
    #[cfg(not(kani))]
    unsafe {
        out.set_len(len);
    }

    out
}

/// Transposes an 8x8 bit matrix stored in a 64-bit integer using the Delta-Swap algorithm.
///
/// Portability & Architecture Notes:
/// - x86_64: Compiles down to branchless single-cycle bitwise instructions (xor, shr, and, shl).
///   Avoids complex AVX2/BMI2 table setup and works uniformly on all x86_64 microarchitectures.
/// - AArch64 (ARM64): Compiles to ~15 fast native A64 ALU instructions (eor, lsr, lsl, and).
/// - Involution property: Transposing twice returns the original word (T(T(x)) == x),
///   so both encoding and decoding share this exact symmetric primitive.
#[inline]
pub fn transpose_8x8_u64(mut x: u64) -> u64 {
    // Fast path: all zeros or all ones are invariant under transposition
    if x == 0 || x == u64::MAX {
        return x;
    }
    let mut t;
    // Step 1: Swap bit-plane 0 and bit-plane 3 (delta = 7)
    t = (x ^ (x >> 7)) & 0x00AA00AA00AA00AA;
    x = x ^ t ^ (t << 7);
    // Step 2: Swap bit-plane 1 and bit-plane 4 (delta = 14)
    t = (x ^ (x >> 14)) & 0x0000CCCC0000CCCC;
    x = x ^ t ^ (t << 14);
    // Step 3: Swap bit-plane 2 and bit-plane 5 (delta = 28)
    t = (x ^ (x >> 28)) & 0x00000000F0F0F0F0;
    x = x ^ t ^ (t << 28);
    x
}

/// In-place 8x8 bit-block transposition over a byte slice.
///
/// Transposes each 8-byte word via [`transpose_8x8_u64`], unrolled 2x (16 bytes)
/// to maximize instruction-level parallelism across superscalar ALU pipes.
/// Trailing `< 8` bytes are left unchanged.
#[inline]
pub fn transpose_bit_blocks_inplace(data: &mut [u8]) {
    let n_blocks = data.len() / 8;
    if n_blocks == 0 {
        return;
    }
    let (chunks_16, rem_16) = data[..n_blocks * 8].as_chunks_mut::<16>();
    for chunk in chunks_16 {
        let w0 = u64::from_le_bytes(chunk[0..8].try_into().unwrap());
        let w1 = u64::from_le_bytes(chunk[8..16].try_into().unwrap());
        let t0 = transpose_8x8_u64(w0);
        let t1 = transpose_8x8_u64(w1);
        chunk[0..8].copy_from_slice(&t0.to_le_bytes());
        chunk[8..16].copy_from_slice(&t1.to_le_bytes());
    }
    if rem_16.len() == 8 {
        let w = u64::from_le_bytes(rem_16.try_into().unwrap());
        let t = transpose_8x8_u64(w);
        rem_16.copy_from_slice(&t.to_le_bytes());
    }
}

/// Bit-level matrix transposition (BitShuffle)
pub fn bitshuffle_encode(data: &[u8], typesize: usize) -> Vec<u8> {
    if data.is_empty() || typesize == 0 {
        return data.to_vec();
    }
    // Step 1: Byte-level shuffle (single allocation)
    let mut out = shuffle_encode(data, typesize);
    // Step 2: In-place 8x8 bit-block transposition (zero extra allocation)
    transpose_bit_blocks_inplace(&mut out);
    out
}

/// Reverse bit-level matrix transposition
pub fn bitshuffle_decode(data: &[u8], typesize: usize) -> Vec<u8> {
    if data.is_empty() || typesize == 0 {
        return data.to_vec();
    }
    if typesize == 1 {
        let mut out = data.to_vec();
        transpose_bit_blocks_inplace(&mut out);
        return out;
    }
    let mut unbit = vec![0u8; data.len()];
    let n_bytes = data.len();
    let n_blocks = n_bytes / 8;

    // Transpose 8x8 bit blocks (symmetric operation) using 64-bit word delta swap
    let (in_chunks_16, rem_src_16) = data[..n_blocks * 8].as_chunks::<16>();
    let (out_chunks_16, rem_dst_16) = unbit[..n_blocks * 8].as_chunks_mut::<16>();
    for (src, dst) in in_chunks_16.iter().zip(out_chunks_16.iter_mut()) {
        let w0 = u64::from_le_bytes(src[0..8].try_into().unwrap());
        let w1 = u64::from_le_bytes(src[8..16].try_into().unwrap());
        let t0 = transpose_8x8_u64(w0);
        let t1 = transpose_8x8_u64(w1);
        dst[0..8].copy_from_slice(&t0.to_le_bytes());
        dst[8..16].copy_from_slice(&t1.to_le_bytes());
    }
    if rem_src_16.len() == 8 {
        let w = u64::from_le_bytes(rem_src_16.try_into().unwrap());
        let t = transpose_8x8_u64(w);
        rem_dst_16.copy_from_slice(&t.to_le_bytes());
    }

    let rem_start = n_blocks * 8;
    if rem_start < n_bytes {
        unbit[rem_start..].copy_from_slice(&data[rem_start..]);
    }
    // Reverse byte-level shuffle
    shuffle_decode(&unbit, typesize)
}

/// In-place mantissa precision truncation for floating point numbers
pub fn trunc_precision_encode_inplace(data: &mut [u8], typesize: usize, prec_bits: i8) {
    if data.is_empty() || prec_bits <= 0 {
        return;
    }
    match typesize {
        4 => {
            // f32: 1 sign, 8 exponent, 23 mantissa
            let bits_to_keep = (prec_bits as usize).min(23);
            let bits_to_zero = 23 - bits_to_keep;
            if bits_to_zero > 0 {
                let mask = !((1u32 << bits_to_zero) - 1);
                let n = data.len() / 4;
                #[cfg(target_endian = "little")]
                {
                    let ptr = data.as_mut_ptr();
                    if (ptr as usize).is_multiple_of(std::mem::align_of::<u32>()) {
                        let slice = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u32, n) };
                        for val in slice.iter_mut() {
                            *val &= mask;
                        }
                        return;
                    }
                }
                for i in 0..n {
                    let val = u32::from_le_bytes(data[i * 4..i * 4 + 4].try_into().unwrap());
                    data[i * 4..i * 4 + 4].copy_from_slice(&(val & mask).to_le_bytes());
                }
            }
        }
        8 => {
            // f64: 1 sign, 11 exponent, 52 mantissa
            let bits_to_keep = (prec_bits as usize).min(52);
            let bits_to_zero = 52 - bits_to_keep;
            if bits_to_zero > 0 {
                let mask = !((1u64 << bits_to_zero) - 1);
                let n = data.len() / 8;
                #[cfg(target_endian = "little")]
                {
                    let ptr = data.as_mut_ptr();
                    if (ptr as usize).is_multiple_of(std::mem::align_of::<u64>()) {
                        let slice = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u64, n) };
                        for val in slice.iter_mut() {
                            *val &= mask;
                        }
                        return;
                    }
                }
                for i in 0..n {
                    let val = u64::from_le_bytes(data[i * 8..i * 8 + 8].try_into().unwrap());
                    data[i * 8..i * 8 + 8].copy_from_slice(&(val & mask).to_le_bytes());
                }
            }
        }
        _ => {}
    }
}

/// Truncate mantissa precision for floating point numbers (lossy compression filter)
pub fn trunc_precision_encode(data: &[u8], typesize: usize, prec_bits: i8) -> Vec<u8> {
    let mut out = data.to_vec();
    trunc_precision_encode_inplace(&mut out, typesize, prec_bits);
    out
}

/// Apply the forward filter pipeline according to FilterConfig with zero allocation when inactive
pub fn apply_filters_cow<'a>(data: &'a [u8], config: &FilterConfig) -> Cow<'a, [u8]> {
    if data.is_empty() || !config.is_active() || !matches!(config.typesize, 1 | 2 | 4 | 8) {
        return Cow::Borrowed(data);
    }
    Cow::Owned(config.to_pipeline().apply(data))
}

/// Apply the forward filter pipeline according to FilterConfig
pub fn apply_filters(data: &[u8], config: &FilterConfig) -> Vec<u8> {
    apply_filters_cow(data, config).into_owned()
}

/// Apply the reverse filter pipeline according to FilterConfig with zero allocation when inactive
pub fn unapply_filters_cow<'a>(data: &'a [u8], config: &FilterConfig) -> Cow<'a, [u8]> {
    if data.is_empty() || !config.is_active() || !matches!(config.typesize, 1 | 2 | 4 | 8) {
        return Cow::Borrowed(data);
    }
    Cow::Owned(config.to_pipeline().unapply(data))
}

/// Apply the reverse filter pipeline according to FilterConfig
pub fn unapply_filters(data: &[u8], config: &FilterConfig) -> Vec<u8> {
    unapply_filters_cow(data, config).into_owned()
}

/// Apply the reverse filter pipeline in-place on an owned buffer, avoiding
/// an extra allocation for in-place filters (e.g. `Delta`).
pub fn unapply_filters_in_place(data: &mut Vec<u8>, config: &FilterConfig) {
    if data.is_empty() || !config.is_active() || !matches!(config.typesize, 1 | 2 | 4 | 8) {
        return;
    }
    config.to_pipeline().unapply_in_place(data);
}

/// Compute Shannon entropy in bits per byte (0.0 to 8.0)
pub fn calculate_entropy(data: &[u8]) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    for &byte in data {
        counts[byte as usize] += 1;
    }
    let len_f = data.len() as f64;
    let mut entropy = 0.0;
    for &c in &counts {
        if c > 0 {
            let p = c as f64 / len_f;
            entropy -= p * p.log2();
        }
    }
    entropy
}

/// Evaluation report for a single filter candidate configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterCandidateReport {
    pub config: FilterConfig,
    pub label: String,
    pub entropy: f64,
    pub compressed_size: usize,
    pub compression_ratio: f64,
    pub space_savings_percent: f64,
}

/// Result of filter recommendation analysis
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterRecommendation {
    pub original_size: usize,
    pub baseline_entropy: f64,
    pub baseline_compressed_size: usize,
    pub best_config: FilterConfig,
    pub best_report: FilterCandidateReport,
    pub candidates: Vec<FilterCandidateReport>,
}

/// Analyze data against various filter configurations and recommend the optimal one.
///
/// Uses representative multi-window stride sampling for large files (> 256 KB)
/// and Rayon data parallelism across candidate simulations for sub-millisecond recommendation.
pub fn recommend_filters(data: &[u8]) -> FilterRecommendation {
    const SAMPLE_THRESHOLD: usize = 256 * 1024; // 256 KB
    const SAMPLE_CHUNK_SIZE: usize = 64 * 1024; // 64 KB per chunk

    let original_size = data.len();
    if original_size == 0 {
        let none_cfg = FilterConfig::none();
        let report = FilterCandidateReport {
            config: none_cfg,
            label: "None (Raw)".to_string(),
            entropy: 0.0,
            compressed_size: 0,
            compression_ratio: 1.0,
            space_savings_percent: 0.0,
        };
        return FilterRecommendation {
            original_size: 0,
            baseline_entropy: 0.0,
            baseline_compressed_size: 0,
            best_config: none_cfg,
            best_report: report.clone(),
            candidates: vec![report],
        };
    }

    // Multi-window sampling for files > 256 KB
    let (eval_data, is_sampled): (Cow<[u8]>, bool) = if original_size <= SAMPLE_THRESHOLD {
        (Cow::Borrowed(data), false)
    } else {
        // Sample head 64KB, middle 64KB, tail 64KB (each aligned to 8 bytes for filter integrity)
        let mut sample = Vec::with_capacity(SAMPLE_CHUNK_SIZE * 3);
        sample.extend_from_slice(&data[..SAMPLE_CHUNK_SIZE]);

        let mid_start = ((original_size / 2) / 8) * 8;
        sample.extend_from_slice(&data[mid_start..mid_start + SAMPLE_CHUNK_SIZE]);

        let tail_start = ((original_size - SAMPLE_CHUNK_SIZE) / 8) * 8;
        sample.extend_from_slice(&data[tail_start..tail_start + SAMPLE_CHUNK_SIZE]);
        (Cow::Owned(sample), true)
    };

    let baseline_entropy = calculate_entropy(&eval_data);
    let baseline_eval_comp = crate::zstd_pool::compress_frame(&eval_data, 0)
        .map(|v| v.len())
        .unwrap_or(eval_data.len());

    let scale_factor = if is_sampled {
        original_size as f64 / eval_data.len() as f64
    } else {
        1.0
    };

    let baseline_comp = if is_sampled {
        (baseline_eval_comp as f64 * scale_factor) as usize
    } else {
        baseline_eval_comp
    };

    let filtered_candidates: [(FilterConfig, &str); 13] = [
        (FilterConfig::delta_only(1), "Delta (typesize=1, u8)"),
        (FilterConfig::delta_only(2), "Delta (typesize=2, u16)"),
        (FilterConfig::delta_only(4), "Delta (typesize=4, u32/f32)"),
        (FilterConfig::delta_only(8), "Delta (typesize=8, u64/f64)"),
        (FilterConfig::shuffle_only(2), "Shuffle (typesize=2, u16)"),
        (
            FilterConfig::shuffle_only(4),
            "Shuffle (typesize=4, u32/f32)",
        ),
        (
            FilterConfig::shuffle_only(8),
            "Shuffle (typesize=8, u64/f64)",
        ),
        (
            FilterConfig::bitshuffle_only(2),
            "BitShuffle (typesize=2, u16)",
        ),
        (
            FilterConfig::bitshuffle_only(4),
            "BitShuffle (typesize=4, u32/f32)",
        ),
        (
            FilterConfig::bitshuffle_only(8),
            "BitShuffle (typesize=8, u64/f64)",
        ),
        (FilterConfig::numeric(2), "Delta+Shuffle (typesize=2, u16)"),
        (
            FilterConfig::numeric(4),
            "Delta+Shuffle (typesize=4, u32/f32)",
        ),
        (
            FilterConfig::numeric(8),
            "Delta+Shuffle (typesize=8, u64/f64)",
        ),
    ];

    let eval_slice = eval_data.as_ref();
    let baseline_ratio = if baseline_eval_comp > 0 {
        eval_slice.len() as f64 / baseline_eval_comp as f64
    } else {
        1.0
    };
    let baseline_savings = if !eval_slice.is_empty() {
        (1.0 - (baseline_eval_comp as f64 / eval_slice.len() as f64)) * 100.0
    } else {
        0.0
    };
    let baseline_report = FilterCandidateReport {
        config: FilterConfig::none(),
        label: "None (Raw Zstd)".to_string(),
        entropy: baseline_entropy,
        compressed_size: baseline_comp,
        compression_ratio: baseline_ratio,
        space_savings_percent: baseline_savings,
    };

    // Parallel evaluation on persistent zstd worker pool + recycled Zstd CCtxs.
    struct SyncSlots<T>(Vec<std::cell::UnsafeCell<Option<T>>>);
    // SAFETY: `par_for_index` guarantees each index `idx` runs on at most one thread.
    unsafe impl<T: Send> Sync for SyncSlots<T> {}
    impl<T> SyncSlots<T> {
        #[inline]
        unsafe fn set(&self, idx: usize, val: T) {
            unsafe {
                *self.0[idx].get() = Some(val);
            }
        }
    }
    let slots: SyncSlots<FilterCandidateReport> = SyncSlots(
        (0..filtered_candidates.len())
            .map(|_| std::cell::UnsafeCell::new(None))
            .collect(),
    );
    let slots_ref = &slots;

    crate::zstd_pool::par_for_index(filtered_candidates.len(), |idx| {
        let (cfg, label) = &filtered_candidates[idx];
        let filtered = apply_filters(eval_slice, cfg);
        let entropy = calculate_entropy(&filtered);
        let eval_comp_size = crate::zstd_pool::compress_frame(&filtered, 0)
            .map(|v| v.len())
            .unwrap_or(filtered.len());

        let comp_size = if is_sampled {
            (eval_comp_size as f64 * scale_factor) as usize
        } else {
            eval_comp_size
        };

        let ratio = if eval_comp_size > 0 {
            eval_slice.len() as f64 / eval_comp_size as f64
        } else {
            1.0
        };
        let savings = if !eval_slice.is_empty() {
            (1.0 - (eval_comp_size as f64 / eval_slice.len() as f64)) * 100.0
        } else {
            0.0
        };

        let report = FilterCandidateReport {
            config: *cfg,
            label: (*label).to_string(),
            entropy,
            compressed_size: comp_size,
            compression_ratio: ratio,
            space_savings_percent: savings,
        };
        // SAFETY: `idx` is uniquely assigned to this worker.
        unsafe {
            slots_ref.set(idx, report);
        }
    });

    let mut reports = Vec::with_capacity(filtered_candidates.len() + 1);
    reports.push(baseline_report);
    for cell in slots.0 {
        reports.push(cell.into_inner().expect("filter candidate slot populated"));
    }

    let mut best_idx = 0;
    let mut min_size = baseline_comp;
    for (i, r) in reports.iter().enumerate() {
        if r.compressed_size < min_size {
            min_size = r.compressed_size;
            best_idx = i;
        }
    }

    FilterRecommendation {
        original_size,
        baseline_entropy,
        baseline_compressed_size: baseline_comp,
        best_config: reports[best_idx].config,
        best_report: reports[best_idx].clone(),
        candidates: reports,
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    #[kani::proof]
    fn proof_delta_roundtrip_u32() {
        let mut data = [0u8; 16];
        for i in 0..16 {
            data[i] = kani::any();
        }

        let encoded = delta_encode(&data, 4);
        let decoded = delta_decode(&encoded, 4);

        assert_eq!(data, decoded.as_slice());
    }

    #[kani::proof]
    fn proof_shuffle_roundtrip() {
        let mut data = [0u8; 8];
        for i in 0..8 {
            data[i] = kani::any();
        }

        let encoded = shuffle_encode(&data, 2);
        let decoded = shuffle_decode(&encoded, 2);

        assert_eq!(data, decoded.as_slice());
    }

    #[kani::proof]
    fn proof_shuffle_roundtrip_4byte() {
        let mut data = [0u8; 8];
        for i in 0..8 {
            data[i] = kani::any();
        }

        let encoded = shuffle_encode(&data, 4);
        let decoded = shuffle_decode(&encoded, 4);

        assert_eq!(data, decoded.as_slice());
    }

    #[kani::proof]
    fn proof_full_pipeline_roundtrip() {
        let mut data = [0u8; 4];
        for i in 0..4 {
            data[i] = kani::any();
        }
        let config = FilterConfig::numeric(2);
        let encoded = apply_filters(&data, &config);
        let decoded = unapply_filters(&encoded, &config);

        assert_eq!(data, decoded.as_slice());
    }

    #[kani::proof]
    fn proof_delta_wrapping_extremes() {
        let data: [u8; 2] = [255, 0];
        let encoded = delta_encode(&data, 1);
        // encoded[0] = 255
        // encoded[1] = 0.wrapping_sub(255) = 1
        assert_eq!(encoded, [255, 1]);

        let decoded = delta_decode(&encoded, 1);
        assert_eq!(decoded, [255, 0]);

        let data2: [u8; 2] = [0, 255];
        let encoded2 = delta_encode(&data2, 1);
        // encoded2[0] = 0
        // encoded2[1] = 255.wrapping_sub(0) = 255
        assert_eq!(encoded2, [0, 255]);

        let decoded2 = delta_decode(&encoded2, 1);
        assert_eq!(decoded2, [0, 255]);
    }

    #[kani::proof]
    fn proof_delta_only_roundtrip() {
        let mut data = [0u8; 8];
        for i in 0..8 {
            data[i] = kani::any();
        }
        let config = FilterConfig::delta_only(2);
        let encoded = apply_filters(&data, &config);
        let decoded = unapply_filters(&encoded, &config);
        assert_eq!(data, decoded.as_slice());
    }

    #[kani::proof]
    fn proof_shuffle_only_roundtrip() {
        let mut data = [0u8; 8];
        for i in 0..8 {
            data[i] = kani::any();
        }
        let config = FilterConfig::shuffle_only(4);
        let encoded = apply_filters(&data, &config);
        let decoded = unapply_filters(&encoded, &config);
        assert_eq!(data, decoded.as_slice());
    }

    #[kani::proof]
    fn proof_unaligned_tail_roundtrip() {
        // 5 bytes with typesize=2 has 1 tail byte
        let mut data = [0u8; 5];
        for i in 0..5 {
            data[i] = kani::any();
        }
        let config = FilterConfig::numeric(2);
        let encoded = apply_filters(&data, &config);
        let decoded = unapply_filters(&encoded, &config);
        assert_eq!(data, decoded.as_slice());
    }

    #[kani::proof]
    fn proof_bitshuffle_roundtrip() {
        let mut data = [0u8; 8];
        for i in 0..8 {
            data[i] = kani::any();
        }
        let encoded = bitshuffle_encode(&data, 2);
        let decoded = bitshuffle_decode(&encoded, 2);
        assert_eq!(data, decoded.as_slice());
    }

    #[kani::proof]
    fn proof_pipeline_composite() {
        let mut data = [0u8; 8];
        for i in 0..8 {
            data[i] = kani::any();
        }
        let pipeline = FilterPipeline::new(2)
            .then(FilterType::Delta)
            .then(FilterType::ByteShuffle);
        let encoded = pipeline.apply(&data);
        let decoded = pipeline.unapply(&encoded);
        assert_eq!(data, decoded.as_slice());
    }

    /// Prove that transpose_8x8_u64 is a strict mathematical involution: T(T(x)) == x
    /// for all 2^64 possible 64-bit inputs.
    #[kani::proof]
    fn proof_transpose_8x8_u64_involution() {
        let x: u64 = kani::any();
        let t = transpose_8x8_u64(x);
        let tt = transpose_8x8_u64(t);
        assert_eq!(x, tt, "transpose_8x8_u64 must be an involution");
    }

    /// Prove delta encode and decode roundtrip on u16 elements across arbitrary symbolic bytes.
    #[kani::proof]
    fn proof_delta_roundtrip_u16() {
        let mut data = [0u8; 8];
        for i in 0..8 {
            data[i] = kani::any();
        }
        let mut encoded = data;
        delta_encode_inplace(&mut encoded, 2);
        let mut decoded = encoded;
        delta_decode_inplace(&mut decoded, 2);
        assert_eq!(data, decoded);
    }

    /// Prove delta encode and decode roundtrip on u64 elements across arbitrary symbolic bytes.
    #[kani::proof]
    fn proof_delta_roundtrip_u64() {
        let mut data = [0u8; 16];
        for i in 0..16 {
            data[i] = kani::any();
        }
        let mut encoded = data;
        delta_encode_inplace(&mut encoded, 8);
        let mut decoded = encoded;
        delta_decode_inplace(&mut decoded, 8);
        assert_eq!(data, decoded);
    }

    /// Prove that truncating 0 bits is an exact mathematical identity.
    #[kani::proof]
    fn proof_trunc_precision_zero_bits_identity() {
        let mut data = [0u8; 8];
        for i in 0..8 {
            data[i] = kani::any();
        }
        let mut copy = data;
        trunc_precision_encode_inplace(&mut copy, 4, 0);
        assert_eq!(data, copy);
    }

    /// Prove that delta encoding and decoding with typesize == 0 is safe, never panics with divide-by-zero,
    /// and leaves the data unmodified.
    #[kani::proof]
    fn proof_delta_zero_typesize_panic_free() {
        let mut data = [0u8; 8];
        for i in 0..8 {
            data[i] = kani::any();
        }
        let original = data;
        delta_encode_inplace(&mut data, 0);
        assert_eq!(
            data, original,
            "delta_encode_inplace with typesize 0 must be a no-op"
        );
        delta_decode_inplace(&mut data, 0);
        assert_eq!(
            data, original,
            "delta_decode_inplace with typesize 0 must be a no-op"
        );
    }

    /// Prove that delta encoding and decoding with ANY symbolic typesize never panics.
    #[kani::proof]
    #[kani::unwind(9)]
    fn proof_delta_arbitrary_typesize_panic_free() {
        let mut data = [0u8; 8];
        for i in 0..8 {
            data[i] = kani::any();
        }
        let typesize: usize = kani::any();
        delta_encode_inplace(&mut data, typesize);
        delta_decode_inplace(&mut data, typesize);
    }
}
