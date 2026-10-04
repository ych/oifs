use oifs::OifsSession;
use oifs::disk::CompressionMode;
use oifs::filters::{FilterConfig, calculate_entropy, recommend_filters};
use std::fs;
use std::path::Path;

#[test]
fn test_filter_pipeline_roundtrips() {
    let img_path = "test_filter_pipeline.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let session = OifsSession::open(img_path, 10 * 1024 * 1024).expect("Failed to create session");
    let root = session.resolve_path(".").unwrap();

    // 1. Write structured sequential 32-bit integers: [0, 1, 2, ..., 1023] (4096 bytes)
    let mut num_data = Vec::with_capacity(1024 * 4);
    for i in 0..1024u32 {
        num_data.extend_from_slice(&i.to_le_bytes());
    }

    // Write with delta only (typesize = 4)
    let fid_delta = session.create_file(root, "seq_delta.bin").unwrap();
    session
        .write_data_with_filters(
            fid_delta,
            0,
            &num_data,
            CompressionMode::Always,
            FilterConfig::delta_only(4),
        )
        .unwrap();
    let read_delta = session.read_data(fid_delta).unwrap();
    assert_eq!(read_delta, num_data, "Delta-only read mismatch");

    // Write with shuffle only (typesize = 4)
    let fid_shuffle = session.create_file(root, "seq_shuffle.bin").unwrap();
    session
        .write_data_with_filters(
            fid_shuffle,
            0,
            &num_data,
            CompressionMode::Always,
            FilterConfig::shuffle_only(4),
        )
        .unwrap();
    let read_shuffle = session.read_data(fid_shuffle).unwrap();
    assert_eq!(read_shuffle, num_data, "Shuffle-only read mismatch");

    // Write with full numeric pipeline (Delta + Shuffle, typesize = 4)
    let fid_both = session.create_file(root, "seq_both.bin").unwrap();
    session
        .write_data_with_filters(
            fid_both,
            0,
            &num_data,
            CompressionMode::Always,
            FilterConfig::numeric(4),
        )
        .unwrap();
    let read_both = session.read_data(fid_both).unwrap();
    assert_eq!(read_both, num_data, "Numeric (delta+shuffle) read mismatch");

    // Write with no filters (baseline)
    let fid_raw = session.create_file(root, "seq_raw.bin").unwrap();
    session
        .write_data(fid_raw, 0, &num_data, CompressionMode::Always)
        .unwrap();
    let read_raw = session.read_data(fid_raw).unwrap();
    assert_eq!(read_raw, num_data, "Raw read mismatch");

    drop(session);
    let _ = fs::remove_file(img_path);
}

#[test]
fn test_filter_recommendation_tool() {
    // Generate sequential 32-bit data
    let mut seq_data = Vec::with_capacity(2048 * 4);
    for i in 0..2048u32 {
        seq_data.extend_from_slice(&i.to_le_bytes());
    }

    let raw_entropy = calculate_entropy(&seq_data);
    let rec = recommend_filters(&seq_data);

    // Delta should dramatically lower entropy for sequential data
    assert!(
        rec.best_config.delta,
        "Recommendation tool should select delta for linear series"
    );
    assert_eq!(
        rec.best_config.typesize, 4,
        "Recommendation tool should detect 4-byte typesize"
    );
    assert!(
        rec.best_report.compressed_size <= rec.baseline_compressed_size,
        "Recommended filter must achieve <= compressed size than raw zstd"
    );
    assert!(
        rec.best_report.entropy < raw_entropy,
        "Recommended filter should reduce entropy"
    );

    println!("Recommendation summary:");
    println!(
        "  Baseline size: {} B (compressed: {} B, entropy: {:.3})",
        rec.original_size, rec.baseline_compressed_size, rec.baseline_entropy
    );
    println!(
        "  Best: {} (compressed: {} B, entropy: {:.3}, savings: {:.1}%)",
        rec.best_report.label,
        rec.best_report.compressed_size,
        rec.best_report.entropy,
        rec.best_report.space_savings_percent
    );
}

#[test]
fn test_encrypted_filesystem_with_filters() {
    let img_path = "test_encrypted_filters.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let session = OifsSession::create_encrypted(img_path, 10 * 1024 * 1024, "secure_pass_123")
        .expect("Failed to create encrypted session");
    let root = session.resolve_path(".").unwrap();

    // Data with floating point values (f32)
    let mut float_data = Vec::with_capacity(1000 * 4);
    for i in 0..1000 {
        let val = (i as f32) * 0.125;
        float_data.extend_from_slice(&val.to_le_bytes());
    }

    let fid = session.create_file(root, "floats.bin").unwrap();
    session
        .write_data_with_filters(
            fid,
            0,
            &float_data,
            CompressionMode::Always,
            FilterConfig::numeric(4),
        )
        .unwrap();

    let read_back = session.read_data(fid).unwrap();
    assert_eq!(read_back, float_data);

    drop(session);
    let _ = fs::remove_file(img_path);
}

#[test]
fn test_composite_filter_pipeline() {
    use oifs::filters::{FilterPipeline, FilterType};

    let mut data = Vec::with_capacity(1024 * 4);
    for i in 0..1024u32 {
        data.extend_from_slice(&i.to_le_bytes());
    }

    // Pipeline chaining: Delta -> ByteShuffle -> BitShuffle
    let pipeline = FilterPipeline::new(4)
        .then(FilterType::Delta)
        .then(FilterType::ByteShuffle)
        .then(FilterType::BitShuffle);

    let filtered = pipeline.apply(&data);
    assert_ne!(filtered, data, "Data must be transformed by the pipeline");

    let restored = pipeline.unapply(&filtered);
    assert_eq!(
        restored, data,
        "Composite pipeline roundtrip must match perfectly"
    );
}

#[test]
fn test_native_blosc2_integration() {
    use oifs::filters::{blosc2_compress, blosc2_decompress};

    let mut data = Vec::with_capacity(2048 * 4);
    for i in 0..2048u32 {
        data.extend_from_slice(&i.to_le_bytes());
    }

    // Use native Blosc2 with BitShuffle filter and LZ4 codec
    let compressed = blosc2_compress(
        &data,
        4,
        &[blosc2::Filter::BitShuffle],
        blosc2::CompressAlgo::Lz4,
        5,
    )
    .expect("Native Blosc2 compression failed");

    println!(
        "Native Blosc2 compressed {} bytes to {} bytes",
        data.len(),
        compressed.len()
    );
    assert!(
        compressed.len() < data.len(),
        "Compression must reduce size"
    );

    let decompressed = blosc2_decompress(&compressed).expect("Native Blosc2 decompression failed");
    assert_eq!(
        decompressed, data,
        "Native Blosc2 roundtrip must match original data"
    );
}

#[test]
fn test_delta_typesize_zero_safe() {
    let mut data = vec![1, 2, 3, 4, 5, 6, 7, 8];
    oifs::filters::delta_encode_inplace(&mut data, 0);
    assert_eq!(data, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    oifs::filters::delta_decode_inplace(&mut data, 0);
    assert_eq!(data, vec![1, 2, 3, 4, 5, 6, 7, 8]);

    // Also test public non-inplace wrappers
    let enc = oifs::filters::delta_encode(&data, 0);
    assert_eq!(enc, data);
    let dec = oifs::filters::delta_decode(&enc, 0);
    assert_eq!(dec, data);
}
