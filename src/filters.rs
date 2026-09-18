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
pub fn delta_encode_inplace(data: &mut [u8], typesize: usize) {
    let n = data.len() / typesize;
    if n <= 1 {
        return;
    }

    match typesize {
        1 => {
            for i in (1..n).rev() {
                data[i] = data[i].wrapping_sub(data[i - 1]);
            }
        }
        2 => {
            for i in (1..n).rev() {
                let curr = u16::from_le_bytes([data[i * 2], data[i * 2 + 1]]);
                let prev = u16::from_le_bytes([data[(i - 1) * 2], data[(i - 1) * 2 + 1]]);
                let diff = curr.wrapping_sub(prev).to_le_bytes();
                data[i * 2] = diff[0];
                data[i * 2 + 1] = diff[1];
            }
        }
        4 => {
            for i in (1..n).rev() {
                let curr = u32::from_le_bytes([
                    data[i * 4],
                    data[i * 4 + 1],
                    data[i * 4 + 2],
                    data[i * 4 + 3],
                ]);
                let prev = u32::from_le_bytes([
                    data[(i - 1) * 4],
                    data[(i - 1) * 4 + 1],
                    data[(i - 1) * 4 + 2],
                    data[(i - 1) * 4 + 3],
                ]);
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
pub fn delta_decode_inplace(data: &mut [u8], typesize: usize) {
    let n = data.len() / typesize;
    if n <= 1 {
        return;
    }

    match typesize {
        1 => {
            for i in 1..n {
                data[i] = data[i - 1].wrapping_add(data[i]);
            }
        }
        2 => {
            for i in 1..n {
                let prev = u16::from_le_bytes([data[(i - 1) * 2], data[(i - 1) * 2 + 1]]);
                let curr = u16::from_le_bytes([data[i * 2], data[i * 2 + 1]]);
                let sum = prev.wrapping_add(curr).to_le_bytes();
                data[i * 2] = sum[0];
                data[i * 2 + 1] = sum[1];
            }
        }
        4 => {
            for i in 1..n {
                let prev = u32::from_le_bytes([
                    data[(i - 1) * 4],
                    data[(i - 1) * 4 + 1],
                    data[(i - 1) * 4 + 2],
                    data[(i - 1) * 4 + 3],
                ]);
                let curr = u32::from_le_bytes([
                    data[i * 4],
                    data[i * 4 + 1],
                    data[i * 4 + 2],
                    data[i * 4 + 3],
                ]);
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

    // Step 2: Transpose 8x8 bit blocks
    for b in 0..n_blocks {
        let block_offset = b * 8;
        for i in 0..8 {
            let byte_val = byte_shuffled[block_offset + i];
            if byte_val == 0 {
                continue;
            }
            if byte_val == 0xFF {
                for j in 0..8 {
                    out[block_offset + j] |= 1 << i;
                }
                continue;
            }
            for j in 0..8 {
                let bit = (byte_val >> j) & 1;
                out[block_offset + j] |= bit << i;
            }
        }
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

    // Transpose 8x8 bit blocks (symmetric operation)
    for b in 0..n_blocks {
        let block_offset = b * 8;
        for j in 0..8 {
            let byte_val = data[block_offset + j];
            if byte_val == 0 {
                continue;
            }
            if byte_val == 0xFF {
                for i in 0..8 {
                    unbit[block_offset + i] |= 1 << j;
                }
                continue;
            }
            for i in 0..8 {
                let bit = (byte_val >> i) & 1;
                unbit[block_offset + i] |= bit << j;
            }
        }
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

/// Analyze data against various filter configurations and recommend the optimal one
pub fn recommend_filters(data: &[u8]) -> FilterRecommendation {
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

    let baseline_entropy = calculate_entropy(data);
    let baseline_comp = zstd::stream::encode_all(std::io::Cursor::new(data), 0)
        .map(|v| v.len())
        .unwrap_or(original_size);

    let candidates_to_test = [
        (FilterConfig::none(), "None (Raw Zstd)".to_string()),
        (FilterConfig::delta_only(1), "Delta (typesize=1, u8)".to_string()),
        (FilterConfig::delta_only(2), "Delta (typesize=2, u16)".to_string()),
        (FilterConfig::delta_only(4), "Delta (typesize=4, u32/f32)".to_string()),
        (FilterConfig::delta_only(8), "Delta (typesize=8, u64/f64)".to_string()),
        (FilterConfig::shuffle_only(2), "Shuffle (typesize=2, u16)".to_string()),
        (FilterConfig::shuffle_only(4), "Shuffle (typesize=4, u32/f32)".to_string()),
        (FilterConfig::shuffle_only(8), "Shuffle (typesize=8, u64/f64)".to_string()),
        (FilterConfig::bitshuffle_only(2), "BitShuffle (typesize=2, u16)".to_string()),
        (FilterConfig::bitshuffle_only(4), "BitShuffle (typesize=4, u32/f32)".to_string()),
        (FilterConfig::bitshuffle_only(8), "BitShuffle (typesize=8, u64/f64)".to_string()),
        (FilterConfig::numeric(2), "Delta+Shuffle (typesize=2, u16)".to_string()),
        (FilterConfig::numeric(4), "Delta+Shuffle (typesize=4, u32/f32)".to_string()),
        (FilterConfig::numeric(8), "Delta+Shuffle (typesize=8, u64/f64)".to_string()),
    ];

    let mut reports = Vec::with_capacity(candidates_to_test.len());
    let mut best_idx = 0;
    let mut min_size = baseline_comp;

    for (i, (cfg, label)) in candidates_to_test.iter().enumerate() {
        let filtered = apply_filters(data, cfg);
        let entropy = calculate_entropy(&filtered);
        let comp_size = zstd::stream::encode_all(std::io::Cursor::new(&filtered), 0)
            .map(|v| v.len())
            .unwrap_or(filtered.len());

        let ratio = if comp_size > 0 { original_size as f64 / comp_size as f64 } else { 1.0 };
        let savings = if original_size > 0 {
            (1.0 - (comp_size as f64 / original_size as f64)) * 100.0
        } else {
            0.0
        };

        if comp_size < min_size {
            min_size = comp_size;
            best_idx = i;
        }

        reports.push(FilterCandidateReport {
            config: *cfg,
            label: label.clone(),
            entropy,
            compressed_size: comp_size,
            compression_ratio: ratio,
            space_savings_percent: savings,
        });
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
}
