//! On-disk encoding for [`Inode`] records.
//!
//! # Why this module exists
//!
//! The original implementation serialized inodes with `bincode`. That format is
//! *packed* (no alignment padding) and encodes a fieldless `enum` with a 4-byte
//! discriminant, so its 169-byte output does **not** match the 176-byte `#[repr(C)]`
//! in-memory layout of [`Inode`]. The two cannot be used interchangeably, and the
//! 10 bytes of `repr(C)` padding are uninitialized memory.
//!
//! Writing raw `repr(C)` bytes would therefore have been actively unsafe:
//!
//! * every existing image stores inodes in the 169-byte bincode layout, so raw reads
//!   would misparse every field after `mode`;
//! * the padding holds stack garbage, so writes would be non-deterministic and would
//!   leak process memory to disk.
//!
//! # Format v2
//!
//! Format v2 defines the 256-byte inode slot explicitly: little-endian scalars, a
//! 1-byte file-type tag, and **zero-filled padding** from byte 166 to 255. The result
//! is deterministic, reproducible, leak-free, and byte-identical to a plain
//! `memcpy` of the encoded record.
//!
//! ```text
//! [0]        u8     mode (0 = File, 1 = Directory)
//! [1..9]     u64    size                  (logical / uncompressed)
//! [9..17]    u64    compressed_size      (physical, 0 when stored raw)
//! [17..25]   u64    created_at
//! [25..33]   u64    modified_at
//! [33..129]  12*u64 direct/indirect block pointers
//! [129]      u8     encrypted
//! [130..154] 24B    encryption_nonce
//! [154]      u8     filter_typesize
//! [155]      u8     filter_delta
//! [156]      u8     filter_shuffle
//! [157]      u8     filter_bitshuffle
//! [158..166] u64    triple_indirect
//! [166]      u8     record_version        (see below)
//! [167..256] zero padding
//!
//! # Versioning
//!
//! Two independent version numbers gate decoding:
//!
//! * **SuperBlock `format_version`** selects the *decoder family*. `1` means the
//!   historical `bincode` layout, which carries no self-describing version byte;
//!   `>= 2` means the fixed 256-byte record described here.
//! * **`record_version`** (this module) selects the record revision *within* the
//!   fixed family, and is what makes future formats additive.
//!
//! The split matters because a version byte alone cannot bootstrap the v1 -> v2
//! step: a v1 `bincode` record and a v2 record are not reliably distinguishable by
//! inspection, so the superblock has to say which decoder to use. Once inside the
//! fixed family, however, `record_version` lets a future v3 append fields at
//! offsets >= 166 **without moving any existing field**, so a v3 image can be
//! migrated lazily one record at a time instead of requiring a whole-image pass.
//!
//! Consequence for readers: a decoder must ignore bytes at offsets it does not know,
//! and must never assume a record it can decode is the newest one.
//!
//! # Field stability
//!
//! Offsets `[0..166)` are frozen for all revisions >= 2. New fields may only be
//! appended into the reserved tail `[167..256)`; existing offsets must never move,
//! or every previously written record becomes unreadable.

use crate::inode::{FileType, Inode};
use thiserror::Error;

/// Size of one inode slot in the inode table.
pub const INODE_SLOT_SIZE: usize = 256;

/// Byte length of the frozen field region of a format v2 record (`[0..166)`).
///
/// Bytes from here on are version / reserved space, not fields.
pub const INODE_V2_PAYLOAD_LEN: usize = 166;

/// Byte offset of the record-version byte inside a format v2 inode slot.
pub const INODE_V2_RECORD_VERSION_OFFSET: usize = 166;

/// `record_version` written by this implementation.
pub const INODE_V2_RECORD_VERSION: u8 = 2;

/// First byte not yet assigned by any record revision; reserved for future fields.
pub const INODE_V2_RESERVED_START: usize = INODE_V2_RECORD_VERSION_OFFSET + 1;

/// On-disk format version that introduced the fixed 256-byte inode record.
///
/// v1 is the historical `bincode` layout and is read-only: it can be decoded and
/// served, but new writes keep the v1 encoding until [`crate::disk::DiskManager`]
/// upgrades the image. See the migration path in `src/disk.rs`.
pub const INODE_FORMAT_V2: u32 = 2;

/// Errors raised while decoding an inode record.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum InodeFormatError {
    /// The slot was shorter than [`INODE_SLOT_SIZE`].
    #[error("Inode slot is truncated: {got} bytes, need {INODE_SLOT_SIZE}")]
    Truncated {
        /// Bytes actually available.
        got: usize,
    },
    /// The 1-byte file-type tag was not a known variant.
    #[error("Unknown inode file-type tag: {tag}")]
    UnknownFileType {
        /// Offending tag byte.
        tag: u8,
    },
    /// A format v1 (`bincode`) record could not be decoded.
    #[error("Legacy (v1) inode record is malformed: {0}")]
    Legacy(String),
}

// ---------------------------------------------------------------------------
// Little-endian primitive helpers
// ---------------------------------------------------------------------------

#[inline]
fn put_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

#[inline]
fn get_u64(buf: &[u8], off: usize) -> u64 {
    let mut tmp = [0u8; 8];
    tmp.copy_from_slice(&buf[off..off + 8]);
    u64::from_le_bytes(tmp)
}

#[inline]
fn tag_to_file_type(tag: u8) -> Result<FileType, InodeFormatError> {
    match tag {
        0 => Ok(FileType::File),
        1 => Ok(FileType::Directory),
        other => Err(InodeFormatError::UnknownFileType { tag: other }),
    }
}

#[inline]
fn file_type_to_tag(ft: FileType) -> u8 {
    match ft {
        FileType::File => 0,
        FileType::Directory => 1,
    }
}

// ---------------------------------------------------------------------------
// Format v2
// ---------------------------------------------------------------------------

/// Encode `inode` into a fixed 256-byte format v2 record.
///
/// The returned array is fully deterministic: every byte, including the 90 bytes
/// of padding, is explicitly written, so encoding the same inode twice always
/// produces identical output.
pub fn encode_v2(inode: &Inode) -> [u8; INODE_SLOT_SIZE] {
    let mut buf = [0u8; INODE_SLOT_SIZE];

    buf[0] = file_type_to_tag(inode.mode);
    put_u64(&mut buf, 1, inode.size);
    put_u64(&mut buf, 9, inode.compressed_size);
    put_u64(&mut buf, 17, inode.created_at);
    put_u64(&mut buf, 25, inode.modified_at);
    for (i, blk) in inode.blocks.iter().enumerate() {
        put_u64(&mut buf, 33 + i * 8, *blk);
    }
    buf[129] = u8::from(inode.encrypted);
    buf[130..154].copy_from_slice(&inode.encryption_nonce);
    buf[154] = inode.filter_typesize;
    buf[155] = u8::from(inode.filter_delta);
    buf[156] = u8::from(inode.filter_shuffle);
    buf[157] = u8::from(inode.filter_bitshuffle);
    put_u64(&mut buf, 158, inode.triple_indirect);
    buf[INODE_V2_RECORD_VERSION_OFFSET] = INODE_V2_RECORD_VERSION;
    // buf[INODE_V2_RESERVED_START..256] stays zero.

    debug_assert_eq!(INODE_V2_PAYLOAD_LEN, 166);
    buf
}

/// Decode a format v2 record from a 256-byte slot.
///
/// Rejects an unknown file-type tag rather than guessing, so a corrupted slot
/// surfaces as an error instead of silently becoming a directory.
pub fn decode_v2(slot: &[u8]) -> Result<Inode, InodeFormatError> {
    if slot.len() < INODE_SLOT_SIZE {
        return Err(InodeFormatError::Truncated { got: slot.len() });
    }
    let s = &slot[..INODE_SLOT_SIZE];

    let mode = tag_to_file_type(s[0])?;
    let mut blocks = [0u64; 12];
    for (i, blk) in blocks.iter_mut().enumerate() {
        *blk = get_u64(s, 33 + i * 8);
    }
    let mut encryption_nonce = [0u8; 24];
    encryption_nonce.copy_from_slice(&s[130..154]);

    Ok(Inode {
        mode,
        size: get_u64(s, 1),
        compressed_size: get_u64(s, 9),
        created_at: get_u64(s, 17),
        modified_at: get_u64(s, 25),
        blocks,
        encrypted: s[129] != 0,
        encryption_nonce,
        filter_typesize: s[154],
        filter_delta: s[155] != 0,
        filter_shuffle: s[156] != 0,
        filter_bitshuffle: s[157] != 0,
        triple_indirect: get_u64(s, 158),
    })
}

/// Serialize `inode` in the historical `bincode` layout, returning just the payload.
///
/// Separate from [`encode_for`] because the legacy write path overlays only this
/// prefix and deliberately leaves the rest of the slot untouched. Preserving that
/// behaviour keeps unmigrated v1 images byte-identical to how they were written.
pub fn encode_v1_payload(inode: &Inode) -> Result<Vec<u8>, InodeFormatError> {
    bincode::serialize(inode).map_err(|e| InodeFormatError::Legacy(e.to_string()))
}

/// Decode a format v1 (`bincode`) record from an inode slot.
///
/// Kept so images written before the format bump stay readable. The slot is padded
/// with stale bytes beyond the serialized length, which `bincode` ignores.
pub fn decode_v1(slot: &[u8]) -> Result<Inode, InodeFormatError> {
    if slot.len() < INODE_SLOT_SIZE {
        return Err(InodeFormatError::Truncated { got: slot.len() });
    }
    bincode::deserialize(&slot[..INODE_SLOT_SIZE])
        .map_err(|e| InodeFormatError::Legacy(e.to_string()))
}

/// Serialize `inode` in the format selected by `format_version`.
///
/// Format v1 is emitted unchanged so that a not-yet-migrated image stays internally
/// consistent; migrating flips the superblock to v2 and rewrites every slot.
pub fn encode_for(
    inode: &Inode,
    format_version: u32,
) -> Result<[u8; INODE_SLOT_SIZE], InodeFormatError> {
    if format_version >= INODE_FORMAT_V2 {
        return Ok(encode_v2(inode));
    }
    let bytes = bincode::serialize(inode).map_err(|e| InodeFormatError::Legacy(e.to_string()))?;
    let mut slot = [0u8; INODE_SLOT_SIZE];
    if bytes.len() > INODE_SLOT_SIZE {
        return Err(InodeFormatError::Legacy(format!(
            "serialized inode is {} bytes, exceeds the {INODE_SLOT_SIZE}-byte slot",
            bytes.len()
        )));
    }
    slot[..bytes.len()].copy_from_slice(&bytes);
    Ok(slot)
}

/// Decode an inode slot using the format indicated by `format_version`.
pub fn decode_for(slot: &[u8], format_version: u32) -> Result<Inode, InodeFormatError> {
    if format_version >= INODE_FORMAT_V2 {
        decode_v2(slot)
    } else {
        decode_v1(slot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Inode {
        let mut i = Inode::new(FileType::Directory);
        i.size = 0x1122_3344_5566_7788;
        i.compressed_size = 0x0102_0304_0506_0708;
        i.created_at = 1_700_000_000;
        i.modified_at = 1_700_000_999;
        for (n, blk) in i.blocks.iter_mut().enumerate() {
            *blk = 0x0F0E_0D0C_0B0A_0900u64.wrapping_add(n as u64);
        }
        i.encrypted = true;
        for (n, b) in i.encryption_nonce.iter_mut().enumerate() {
            *b = n as u8;
        }
        i.filter_typesize = 4;
        i.filter_delta = true;
        i.filter_shuffle = true;
        i.filter_bitshuffle = true;
        i.triple_indirect = 0xDEAD_BEEF_CAFE_BABE;
        i
    }

    #[test]
    fn test_v2_roundtrip_preserves_every_field() {
        let original = sample();
        let slot = encode_v2(&original);
        let decoded = decode_v2(&slot).expect("decode");
        assert_eq!(decoded.mode, original.mode);
        assert_eq!(decoded.size, original.size);
        assert_eq!(decoded.compressed_size, original.compressed_size);
        assert_eq!(decoded.created_at, original.created_at);
        assert_eq!(decoded.modified_at, original.modified_at);
        assert_eq!(decoded.blocks, original.blocks);
        assert_eq!(decoded.encrypted, original.encrypted);
        assert_eq!(decoded.encryption_nonce, original.encryption_nonce);
        assert_eq!(decoded.filter_typesize, original.filter_typesize);
        assert_eq!(decoded.filter_delta, original.filter_delta);
        assert_eq!(decoded.filter_shuffle, original.filter_shuffle);
        assert_eq!(decoded.filter_bitshuffle, original.filter_bitshuffle);
        assert_eq!(decoded.triple_indirect, original.triple_indirect);
    }

    #[test]
    fn test_v2_encoding_is_deterministic() {
        // The whole point of the fixed format: no padding garbage leaks in.
        assert_eq!(encode_v2(&sample()), encode_v2(&sample()));
    }

    #[test]
    fn test_v2_reserved_tail_is_zero_and_versioned() {
        let slot = encode_v2(&sample());
        assert_eq!(
            slot[INODE_V2_RECORD_VERSION_OFFSET], INODE_V2_RECORD_VERSION,
            "record_version must be stamped"
        );
        assert!(
            slot[INODE_V2_RESERVED_START..].iter().all(|b| *b == 0),
            "reserved tail must be deterministically zeroed"
        );
    }

    #[test]
    fn test_v2_ignores_stale_bytes_in_reserved_tail() {
        // A v2 record must decode identically regardless of what sits in the
        // reserved tail, which is what makes in-place rewriting and future
        // additive revisions safe.
        let mut slot = encode_v2(&sample());
        let want = decode_v2(&slot).expect("d");
        slot[INODE_V2_RESERVED_START..].fill(0xEE);
        assert_eq!(decode_v2(&slot).expect("d"), want);
    }

    #[test]
    fn test_v2_decoder_ignores_unknown_higher_record_version() {
        // Forward compatibility: a future revision may append fields. An older
        // decoder must still read the frozen region correctly.
        let mut slot = encode_v2(&sample());
        slot[INODE_V2_RECORD_VERSION_OFFSET] = 200;
        let decoded = decode_v2(&slot).expect("must still decode");
        assert_eq!(decoded.size, sample().size);
        assert_eq!(decoded.triple_indirect, sample().triple_indirect);
    }

    #[test]
    fn test_v2_field_offsets_are_exact() {
        let slot = encode_v2(&sample());
        assert_eq!(
            slot[0], 1,
            "mode tag for Directory (sample() is a directory)"
        );
        assert_eq!(&slot[1..9], &0x1122_3344_5566_7788u64.to_le_bytes());
        assert_eq!(&slot[9..17], &0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(&slot[17..25], &1_700_000_000u64.to_le_bytes());
        assert_eq!(&slot[25..33], &1_700_000_999u64.to_le_bytes());
        assert_eq!(&slot[33..41], &0x0F0E_0D0C_0B0A_0900u64.to_le_bytes());
        assert_eq!(slot[129], 1, "encrypted flag");
        assert_eq!(slot[154], 4, "filter_typesize");
        assert_eq!(slot[155], 1, "filter_delta");
        assert_eq!(slot[156], 1, "filter_shuffle");
        assert_eq!(slot[157], 1, "filter_bitshuffle");
        assert_eq!(&slot[158..166], &0xDEAD_BEEF_CAFE_BABEu64.to_le_bytes());
    }

    #[test]
    fn test_v2_directory_tag() {
        let i = Inode::new(FileType::Directory);
        assert_eq!(encode_v2(&i)[0], 1);
        assert_eq!(
            decode_v2(&encode_v2(&i)).expect("d").mode,
            FileType::Directory
        );
    }

    #[test]
    fn test_v2_rejects_unknown_file_type() {
        let mut slot = encode_v2(&Inode::new(FileType::File));
        slot[0] = 7;
        assert_eq!(
            decode_v2(&slot),
            Err(InodeFormatError::UnknownFileType { tag: 7 })
        );
    }

    #[test]
    fn test_v2_rejects_short_slot() {
        assert_eq!(
            decode_v2(&[0u8; 10]),
            Err(InodeFormatError::Truncated { got: 10 })
        );
    }

    #[test]
    fn test_v2_ignores_stale_bytes_in_padding() {
        // The frozen field region must decode identically regardless of what sits
        // after it, which is what makes the format safe to write in place.
        let mut slot = encode_v2(&sample());
        let want = decode_v2(&slot).expect("d");
        slot[INODE_V2_PAYLOAD_LEN..].fill(0xEE);
        assert_eq!(decode_v2(&slot).expect("d"), want);
    }

    #[test]
    fn test_v1_still_decodes() {
        let i = sample();
        let bytes = bincode::serialize(&i).expect("ser");
        let mut slot = [0xCDu8; INODE_SLOT_SIZE];
        slot[..bytes.len()].copy_from_slice(&bytes);
        let decoded = decode_v1(&slot).expect("v1 decode");
        assert_eq!(decoded.size, i.size);
        assert_eq!(decoded.blocks, i.blocks);
        assert_eq!(decoded.triple_indirect, i.triple_indirect);
    }

    #[test]
    fn test_encode_for_dispatches_on_version() {
        let i = Inode::new(FileType::File);
        // v1 path must produce the bincode layout, v2 the fixed one.
        assert_ne!(
            encode_for(&i, 1).expect("v1"),
            encode_for(&i, 2).expect("v2")
        );
        // Unknown future versions are treated as v2 (forward-compatible read).
        assert_eq!(encode_for(&i, 99).expect("v99"), encode_v2(&i));
    }

    #[test]
    fn test_decode_for_dispatches_on_version() {
        let i = sample();
        let mut slot = [0u8; INODE_SLOT_SIZE];
        let bytes = bincode::serialize(&i).expect("ser");
        slot[..bytes.len()].copy_from_slice(&bytes);
        assert_eq!(decode_for(&slot, 1).expect("v1").size, i.size);
        assert_eq!(decode_for(&encode_v2(&i), 2).expect("v2").size, i.size);
    }

    #[test]
    fn test_v2_is_not_bincode_compatible() {
        // Documents *why* the version bump is required: the two encodings differ,
        // so a v1 image must never be read with the v2 decoder.
        let i = sample();
        let v2 = encode_v2(&i);
        let mut v1slot = [0u8; INODE_SLOT_SIZE];
        let bytes = bincode::serialize(&i).expect("ser");
        v1slot[..bytes.len()].copy_from_slice(&bytes);
        assert_ne!(v2[..bytes.len()], v1slot[..bytes.len()]);
    }
}
