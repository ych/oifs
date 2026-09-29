use oifs::filters::{
    bitshuffle_decode, bitshuffle_encode, delta_decode_inplace, delta_encode_inplace,
    transpose_8x8_u64,
};

/// Reference bit-by-bit transposition implementation from original scalar algorithm
fn reference_bit_transpose(bytes: &[u8; 8]) -> [u8; 8] {
    let mut out = [0u8; 8];
    for i in 0..8 {
        for j in 0..8 {
            let bit = (bytes[i] >> j) & 1;
            out[j] |= bit << i;
        }
    }
    out
}

#[test]
fn test_transpose_8x8_reference_parity_and_involution() {
    // 1. All single-bit positions (64 cases)
    for bit_idx in 0..64 {
        let word = 1u64 << bit_idx;
        let bytes = word.to_le_bytes();
        let expected = reference_bit_transpose(&bytes);
        let actual = transpose_8x8_u64(word).to_le_bytes();
        assert_eq!(
            actual, expected,
            "Bit transpose mismatch at single bit {}",
            bit_idx
        );
        // Involution: T(T(x)) == x
        assert_eq!(
            transpose_8x8_u64(transpose_8x8_u64(word)),
            word,
            "Involution failed at single bit {}",
            bit_idx
        );
    }

    // 2. Invariant words (all 0s and all 1s)
    assert_eq!(transpose_8x8_u64(0), 0);
    assert_eq!(transpose_8x8_u64(u64::MAX), u64::MAX);

    // 3. Diagonal identity matrix: byte i has bit i set
    let mut diag_bytes = [0u8; 8];
    for i in 0..8 {
        diag_bytes[i] = 1 << i;
    }
    let diag_word = u64::from_le_bytes(diag_bytes);
    assert_eq!(
        transpose_8x8_u64(diag_word),
        diag_word,
        "Identity matrix must be invariant under transposition"
    );

    // 4. Anti-diagonal matrix: byte i has bit (7-i) set
    let mut anti_diag = [0u8; 8];
    for i in 0..8 {
        anti_diag[i] = 1 << (7 - i);
    }
    let anti_word = u64::from_le_bytes(anti_diag);
    let anti_transposed = transpose_8x8_u64(anti_word).to_le_bytes();
    assert_eq!(
        anti_transposed,
        reference_bit_transpose(&anti_diag),
        "Anti-diagonal transpose must match reference"
    );

    // 5. 10,000 deterministic pseudorandom patterns
    let mut state = 0x123456789ABCDEF0u64;
    for _ in 0..10_000 {
        // Xorshift64 PRNG
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;

        let bytes = state.to_le_bytes();
        let expected = reference_bit_transpose(&bytes);
        let actual = transpose_8x8_u64(state).to_le_bytes();
        assert_eq!(actual, expected, "Mismatch for word 0x{:016X}", state);
        assert_eq!(
            transpose_8x8_u64(transpose_8x8_u64(state)),
            state,
            "Involution failed for 0x{:016X}",
            state
        );
    }
}

#[test]
fn test_endianness_conversion_portability() {
    // Test that packing bytes using from_le_bytes and writing using to_le_bytes
    // is completely deterministic regardless of machine endianness.
    let raw_bytes: [u8; 8] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80];

    // Explicit Little-Endian conversion
    let word_le = u64::from_le_bytes(raw_bytes);
    let transposed_le = transpose_8x8_u64(word_le);
    let out_le = transposed_le.to_le_bytes();

    // Verify roundtrip on byte slice
    let back_word = u64::from_le_bytes(out_le);
    let untransposed = transpose_8x8_u64(back_word);
    assert_eq!(untransposed.to_le_bytes(), raw_bytes);

    // Test BitShuffle roundtrip on arbitrary length byte payloads (including unaligned remainder)
    for len in [0, 1, 7, 8, 9, 15, 16, 23, 64, 127, 1024, 4096] {
        let payload: Vec<u8> = (0..len).map(|i| (i * 37 % 256) as u8).collect();
        for typesize in [1, 2, 4, 8] {
            let encoded = bitshuffle_encode(&payload, typesize);
            let decoded = bitshuffle_decode(&encoded, typesize);
            assert_eq!(
                decoded, payload,
                "BitShuffle roundtrip failed for len {} typesize {}",
                len, typesize
            );
        }
    }
}

#[test]
fn test_delta_filter_endian_invariance() {
    // Test delta filter on u16, u32, u64 with simulated endianness sequences
    // 1. u16
    let vals_u16: Vec<u16> = (0..100).map(|i| i * 42).collect();
    let mut le_u16: Vec<u8> = vals_u16.iter().flat_map(|v| v.to_le_bytes()).collect();
    delta_encode_inplace(&mut le_u16, 2);
    delta_decode_inplace(&mut le_u16, 2);
    let roundtrip_u16: Vec<u16> = le_u16
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes(c.try_into().unwrap()))
        .collect();
    assert_eq!(roundtrip_u16, vals_u16);

    // 2. u32
    let vals_u32: Vec<u32> = (0..100).map(|i| i * 100_007).collect();
    let mut le_u32: Vec<u8> = vals_u32.iter().flat_map(|v| v.to_le_bytes()).collect();
    delta_encode_inplace(&mut le_u32, 4);
    delta_decode_inplace(&mut le_u32, 4);
    let roundtrip_u32: Vec<u32> = le_u32
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    assert_eq!(roundtrip_u32, vals_u32);

    // 3. u64
    let vals_u64: Vec<u64> = (0..100).map(|i| i * 1_000_000_007).collect();
    let mut le_u64: Vec<u8> = vals_u64.iter().flat_map(|v| v.to_le_bytes()).collect();
    delta_encode_inplace(&mut le_u64, 8);
    delta_decode_inplace(&mut le_u64, 8);
    let roundtrip_u64: Vec<u64> = le_u64
        .chunks_exact(8)
        .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
        .collect();
    assert_eq!(roundtrip_u64, vals_u64);
}

#[test]
fn test_rust_idiomatic_endianness_detection_and_conversions() {
    // 1. Compile-time platform endianness check using idiomatic Rust cfg!
    let is_little_endian = cfg!(target_endian = "little");
    let is_big_endian = cfg!(target_endian = "big");

    // Exactly one must be true on any Rust target
    assert!(is_little_endian ^ is_big_endian);

    if is_little_endian {
        println!("Compiled on Little-Endian platform (x86_64, AArch64 / Apple Silicon)");
    } else {
        println!("Compiled on Big-Endian platform");
    }

    // 2. Standard library integer endianness conversion methods (from_le, to_le, swap_bytes)
    let magic_le = 0x4F494653u32; // "OIFS" in little endian
    let native_magic = u32::from_le(magic_le);
    let back_to_le = native_magic.to_le();
    assert_eq!(back_to_le, magic_le);

    // 3. u64 conversions
    let test_u64 = 0x0123456789ABCDEFu64;
    let le_bytes = test_u64.to_le_bytes();
    let from_bytes = u64::from_le_bytes(le_bytes);
    assert_eq!(from_bytes, test_u64);

    // On little-endian platforms, native bytes match le_bytes exactly
    if is_little_endian {
        assert_eq!(test_u64.to_ne_bytes(), test_u64.to_le_bytes());
        assert_eq!(u64::from_le(test_u64), test_u64);
    }
}

