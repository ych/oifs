use rayon::prelude::*;
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
                FilterType::BitShuffle => cur = bitshuffle_encode(&cur, self.typesize as usize),
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
        for f in self.filters.iter().rev() {
            match f {
                FilterType::Delta => delta_decode_inplace(&mut cur, self.typesize as usize),
                FilterType::ByteShuffle => cur = shuffle_decode(&cur, self.typesize as usize),
                FilterType::BitShuffle => cur = bitshuffle_decode(&cur, self.typesize as usize),
                FilterType::TruncPrecision { .. } => {} // Truncation is lossy; unapply is identity
            }
        }
        cur
    }
}

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

/// In-place first-order delta encoding with zero allocations
#[inline]
pub fn delta_encode_inplace(data: &mut [u8], typesize: usize) {
    let n = data.len() / typesize;
    if n <= 1 {
        return;
    }

    #[cfg(target_endian = "little")]
    {
        // On Little-Endian architectures (x86_64, AArch64 / Apple Silicon), the in-memory
        // byte representation matches on-disk format. If properly aligned, operating directly
        // on typed slices eliminates bounds checks and allows auto-vectorization (AVX2/NEON).
        let ptr = data.as_mut_ptr();
        match typesize {
            1 => {
                for i in (1..n).rev() {
                    data[i] = data[i].wrapping_sub(data[i - 1]);
                }
                return;
            }
            2 if (ptr as usize).is_multiple_of(std::mem::align_of::<u16>()) => {
                let slice = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u16, n) };
                for i in (1..n).rev() {
                    slice[i] = slice[i].wrapping_sub(slice[i - 1]);
                }
                return;
            }
            4 if (ptr as usize).is_multiple_of(std::mem::align_of::<u32>()) => {
                let slice = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u32, n) };
                for i in (1..n).rev() {
                    slice[i] = slice[i].wrapping_sub(slice[i - 1]);
                }
                return;
            }
            8 if (ptr as usize).is_multiple_of(std::mem::align_of::<u64>()) => {
                let slice = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u64, n) };
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
    let n = data.len() / typesize;
    if n <= 1 {
        return;
    }

    #[cfg(target_endian = "little")]
    {
        let ptr = data.as_mut_ptr();
        match typesize {
            1 => {
                for i in 1..n {
                    data[i] = data[i - 1].wrapping_add(data[i]);
                }
                return;
            }
            2 if (ptr as usize).is_multiple_of(std::mem::align_of::<u16>()) => {
                let slice = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u16, n) };
                for i in 1..n {
                    slice[i] = slice[i - 1].wrapping_add(slice[i]);
                }
                return;
            }
            4 if (ptr as usize).is_multiple_of(std::mem::align_of::<u32>()) => {
                let slice = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u32, n) };
                for i in 1..n {
                    slice[i] = slice[i - 1].wrapping_add(slice[i]);
                }
                return;
            }
            8 if (ptr as usize).is_multiple_of(std::mem::align_of::<u64>()) => {
                let slice = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u64, n) };
                for i in 1..n {
                    slice[i] = slice[i - 1].wrapping_add(slice[i]);
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
    let mut out = vec![0; data.len()];
    let n = data.len() / typesize;

    match typesize {
        2 => {
            for i in 0..n {
                out[i] = data[i * 2];
                out[n + i] = data[i * 2 + 1];
            }
        }
        4 => {
            for i in 0..n {
                out[i] = data[i * 4];
                out[n + i] = data[i * 4 + 1];
                out[2 * n + i] = data[i * 4 + 2];
                out[3 * n + i] = data[i * 4 + 3];
            }
        }
        8 => {
            for i in 0..n {
                out[i] = data[i * 8];
                out[n + i] = data[i * 8 + 1];
                out[2 * n + i] = data[i * 8 + 2];
                out[3 * n + i] = data[i * 8 + 3];
                out[4 * n + i] = data[i * 8 + 4];
                out[5 * n + i] = data[i * 8 + 5];
                out[6 * n + i] = data[i * 8 + 6];
                out[7 * n + i] = data[i * 8 + 7];
            }
        }
        _ => {
            for i in 0..n {
                for j in 0..typesize {
                    out[j * n + i] = data[i * typesize + j];
                }
            }
        }
    }

    let tail_start = n * typesize;
    if tail_start < data.len() {
        out[tail_start..].copy_from_slice(&data[tail_start..]);
    }

    out
}

pub fn shuffle_decode(data: &[u8], typesize: usize) -> Vec<u8> {
    if typesize <= 1 || data.is_empty() {
        return data.to_vec();
    }
    let mut out = vec![0; data.len()];
    let n = data.len() / typesize;

    match typesize {
        2 => {
            for i in 0..n {
                out[i * 2] = data[i];
                out[i * 2 + 1] = data[n + i];
            }
        }
        4 => {
            for i in 0..n {
                out[i * 4] = data[i];
                out[i * 4 + 1] = data[n + i];
                out[i * 4 + 2] = data[2 * n + i];
                out[i * 4 + 3] = data[3 * n + i];
            }
        }
        8 => {
            for i in 0..n {
                out[i * 8] = data[i];
                out[i * 8 + 1] = data[n + i];
                out[i * 8 + 2] = data[2 * n + i];
                out[i * 8 + 3] = data[3 * n + i];
                out[i * 8 + 4] = data[4 * n + i];
                out[i * 8 + 5] = data[5 * n + i];
                out[i * 8 + 6] = data[6 * n + i];
                out[i * 8 + 7] = data[7 * n + i];
            }
        }
        _ => {
            for i in 0..n {
                for j in 0..typesize {
                    out[i * typesize + j] = data[j * n + i];
                }
            }
        }
    }

    let tail_start = n * typesize;
    if tail_start < data.len() {
        out[tail_start..].copy_from_slice(&data[tail_start..]);
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

/// Bit-level matrix transposition (BitShuffle)
pub fn bitshuffle_encode(data: &[u8], typesize: usize) -> Vec<u8> {
    if data.is_empty() || typesize == 0 {
        return data.to_vec();
    }
    // Step 1: Byte-level shuffle
    let byte_shuffled = shuffle_encode(data, typesize);
    let mut out = vec![0u8; data.len()];
    let n_bytes = data.len();
    let n_blocks = n_bytes / 8;

    // Step 2: Transpose 8x8 bit blocks using 64-bit word delta swap
    // Unroll 2x (16 bytes) to maximize instruction-level parallelism across superscalar ALU pipes
    let (in_chunks_16, rem_src_16) = byte_shuffled[..n_blocks * 8].as_chunks::<16>();
    let (out_chunks_16, rem_dst_16) = out[..n_blocks * 8].as_chunks_mut::<16>();
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
        out[rem_start..].copy_from_slice(&byte_shuffled[rem_start..]);
    }
    out
}

/// Reverse bit-level matrix transposition
pub fn bitshuffle_decode(data: &[u8], typesize: usize) -> Vec<u8> {
    if data.is_empty() || typesize == 0 {
        return data.to_vec();
    }
    let mut unbit = vec![0u8; data.len()];
    let n_bytes = data.len();
    let n_blocks = n_bytes / 8;

    // Transpose 8x8 bit blocks (symmetric operation) using 64-bit word delta swap
    // Unroll 2x (16 bytes) to maximize instruction-level parallelism across superscalar ALU pipes
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
/// P2.1 Optimization: Uses representative multi-window stride sampling for large files (> 256 KB)
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

    // P2.1 Optimization: Multi-window sampling for files > 256 KB
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
    let baseline_eval_comp = zstd::stream::encode_all(std::io::Cursor::new(&*eval_data), 0)
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

    let candidates_to_test = [
        (FilterConfig::none(), "None (Raw Zstd)".to_string()),
        (
            FilterConfig::delta_only(1),
            "Delta (typesize=1, u8)".to_string(),
        ),
        (
            FilterConfig::delta_only(2),
            "Delta (typesize=2, u16)".to_string(),
        ),
        (
            FilterConfig::delta_only(4),
            "Delta (typesize=4, u32/f32)".to_string(),
        ),
        (
            FilterConfig::delta_only(8),
            "Delta (typesize=8, u64/f64)".to_string(),
        ),
        (
            FilterConfig::shuffle_only(2),
            "Shuffle (typesize=2, u16)".to_string(),
        ),
        (
            FilterConfig::shuffle_only(4),
            "Shuffle (typesize=4, u32/f32)".to_string(),
        ),
        (
            FilterConfig::shuffle_only(8),
            "Shuffle (typesize=8, u64/f64)".to_string(),
        ),
        (
            FilterConfig::bitshuffle_only(2),
            "BitShuffle (typesize=2, u16)".to_string(),
        ),
        (
            FilterConfig::bitshuffle_only(4),
            "BitShuffle (typesize=4, u32/f32)".to_string(),
        ),
        (
            FilterConfig::bitshuffle_only(8),
            "BitShuffle (typesize=8, u64/f64)".to_string(),
        ),
        (
            FilterConfig::numeric(2),
            "Delta+Shuffle (typesize=2, u16)".to_string(),
        ),
        (
            FilterConfig::numeric(4),
            "Delta+Shuffle (typesize=4, u32/f32)".to_string(),
        ),
        (
            FilterConfig::numeric(8),
            "Delta+Shuffle (typesize=8, u64/f64)".to_string(),
        ),
    ];

    // P2.1 Optimization: Rayon parallel evaluation across CPU cores
    let reports: Vec<FilterCandidateReport> = candidates_to_test
        .par_iter()
        .map(|(cfg, label)| {
            let filtered = apply_filters(&eval_data, cfg);
            let entropy = calculate_entropy(&filtered);
            let eval_comp_size = zstd::stream::encode_all(std::io::Cursor::new(&filtered), 0)
                .map(|v| v.len())
                .unwrap_or(filtered.len());

            let comp_size = if is_sampled {
                (eval_comp_size as f64 * scale_factor) as usize
            } else {
                eval_comp_size
            };

            let ratio = if eval_comp_size > 0 {
                eval_data.len() as f64 / eval_comp_size as f64
            } else {
                1.0
            };
            let savings = if !eval_data.is_empty() {
                (1.0 - (eval_comp_size as f64 / eval_data.len() as f64)) * 100.0
            } else {
                0.0
            };

            FilterCandidateReport {
                config: *cfg,
                label: label.clone(),
                entropy,
                compressed_size: comp_size,
                compression_ratio: ratio,
                space_savings_percent: savings,
            }
        })
        .collect();

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
}
