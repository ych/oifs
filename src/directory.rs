use serde::{Deserialize, Serialize};
use std::io::{Cursor, Read, Write};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DirectoryEntry {
    pub inode: u64,
    pub hash: u64,
    pub name: String,
}

#[derive(Error, Debug)]
pub enum DirectoryError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Encoding error")]
    Utf8Error(#[from] std::string::FromUtf8Error),
    #[error("Entry too large")]
    EntryTooLarge,
}

impl DirectoryEntry {
    pub const MAX_FILENAME_LEN: usize = 255;

    pub fn serialize_into<W: Write>(&self, writer: &mut W) -> Result<(), DirectoryError> {
        if self.name.contains('/') {
            return Err(DirectoryError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Filename cannot contain '/'",
            )));
        }
        let name_bytes = self.name.as_bytes();
        if name_bytes.len() > Self::MAX_FILENAME_LEN {
            return Err(DirectoryError::EntryTooLarge);
        }

        writer.write_all(&self.inode.to_le_bytes())?;
        writer.write_all(&self.hash.to_le_bytes())?;
        writer.write_all(&(name_bytes.len() as u16).to_le_bytes())?;
        writer.write_all(name_bytes)?;

        Ok(())
    }

    pub fn deserialize_from<R: Read>(reader: &mut R) -> Result<Option<Self>, DirectoryError> {
        let mut inode_buf = [0u8; 8];
        if reader.read_exact(&mut inode_buf).is_err() {
            return Ok(None);
        }
        let inode = u64::from_le_bytes(inode_buf);

        let mut hash_buf = [0u8; 8];
        if reader.read_exact(&mut hash_buf).is_err() {
            return Ok(None);
        }
        let hash = u64::from_le_bytes(hash_buf);

        let mut len_buf = [0u8; 2];
        if reader.read_exact(&mut len_buf).is_err() {
            return Ok(None);
        }
        let len = u16::from_le_bytes(len_buf) as usize;

        if len == 0 {
            // Assume 0 length name means end of entries
            return Ok(None);
        }

        let mut name_buf = vec![0u8; len];
        reader.read_exact(&mut name_buf)?;
        let name = String::from_utf8(name_buf)?;
        // If len == 0 -> End.

        // Wait, I cannot read `name` before `len`.
        // So correct flow:
        // read inode, hash, len.
        // if len == 0 -> return None (End).
        // else read name.

        Ok(Some(Self { inode, hash, name }))
    }
}

pub struct DirectoryIterator<'a> {
    cursor: Cursor<&'a [u8]>,
}

impl<'a> DirectoryIterator<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            cursor: Cursor::new(data),
        }
    }
}

impl<'a> Iterator for DirectoryIterator<'a> {
    type Item = Result<DirectoryEntry, DirectoryError>;

    fn next(&mut self) -> Option<Self::Item> {
        // If we are at end of position?
        if self.cursor.position() >= self.cursor.get_ref().len() as u64 {
            return None;
        }

        match DirectoryEntry::deserialize_from(&mut self.cursor) {
            Ok(Some(entry)) => Some(Ok(entry)),
            Ok(None) => None, // End of entries
            Err(e) => Some(Err(e)),
        }
    }
}

#[inline(always)]
fn sip_round(v0: &mut u64, v1: &mut u64, v2: &mut u64, v3: &mut u64) {
    *v0 = v0.wrapping_add(*v1);
    *v1 = v1.rotate_left(13) ^ *v0;
    *v0 = v0.rotate_left(32);

    *v2 = v2.wrapping_add(*v3);
    *v3 = v3.rotate_left(16) ^ *v2;

    *v0 = v0.wrapping_add(*v3);
    *v3 = v3.rotate_left(21) ^ *v0;

    *v2 = v2.wrapping_add(*v1);
    *v1 = v1.rotate_left(17) ^ *v2;
    *v2 = v2.rotate_left(32);
}

/// Deterministic 64-bit SipHash-2-4 for fast directory hash indexing.
#[allow(clippy::chunks_exact_to_as_chunks)]
pub fn hash_filename(name: &str) -> u64 {
    let bytes = name.as_bytes();
    let k0 = 0x0706050403020100u64;
    let k1 = 0x0f0e0d0c0b0a0908u64;

    let mut v0 = k0 ^ 0x736f6d6570736575u64;
    let mut v1 = k1 ^ 0x646f72616e646f6du64;
    let mut v2 = k0 ^ 0x6c7967656e657261u64;
    let mut v3 = k1 ^ 0x7465646279746573u64;

    let chunks = bytes.chunks_exact(8);
    let remainder = chunks.remainder();

    for chunk in chunks {
        let m = u64::from_le_bytes(chunk.try_into().unwrap());
        v3 ^= m;
        sip_round(&mut v0, &mut v1, &mut v2, &mut v3);
        sip_round(&mut v0, &mut v1, &mut v2, &mut v3);
        v0 ^= m;
    }

    let mut last = (bytes.len() as u64) << 56;
    for (i, &b) in remainder.iter().enumerate() {
        last |= (b as u64) << (i * 8);
    }

    v3 ^= last;
    sip_round(&mut v0, &mut v1, &mut v2, &mut v3);
    sip_round(&mut v0, &mut v1, &mut v2, &mut v3);
    v0 ^= last;

    v2 ^= 0xff;
    sip_round(&mut v0, &mut v1, &mut v2, &mut v3);
    sip_round(&mut v0, &mut v1, &mut v2, &mut v3);
    sip_round(&mut v0, &mut v1, &mut v2, &mut v3);
    sip_round(&mut v0, &mut v1, &mut v2, &mut v3);

    v0 ^ v1 ^ v2 ^ v3
}

/// Zero-allocation fast lookup for a directory entry with 64-bit SipHash filter.
/// Avoids string comparison for mismatched entries when hash != 0.
#[inline]
pub fn find_entry_in_block_with_hash(
    slice: &[u8],
    target_name: &str,
    target_hash: u64,
) -> Option<u64> {
    let target_bytes = target_name.as_bytes();
    let mut offset = 0;
    while offset + 18 <= slice.len() {
        let len = u16::from_le_bytes([slice[offset + 16], slice[offset + 17]]) as usize;
        if len == 0 {
            break;
        }
        let name_start = offset + 18;
        let name_end = name_start + len;
        if name_end > slice.len() {
            break;
        }
        let entry_hash = u64::from_le_bytes(slice[offset + 8..offset + 16].try_into().unwrap());
        if (entry_hash == 0 || target_hash == 0 || entry_hash == target_hash)
            && len == target_bytes.len()
            && &slice[name_start..name_end] == target_bytes
        {
            let inode = u64::from_le_bytes(slice[offset..offset + 8].try_into().unwrap());
            return Some(inode);
        }
        offset = name_end;
    }
    None
}

/// Zero-allocation fast lookup for a directory entry in a raw directory block slice.
/// Returns Some(inode_id) if found, None otherwise.
#[inline]
pub fn find_entry_in_block(slice: &[u8], target_name: &str) -> Option<u64> {
    let target_bytes = target_name.as_bytes();
    let mut offset = 0;
    while offset + 18 <= slice.len() {
        let len = u16::from_le_bytes([slice[offset + 16], slice[offset + 17]]) as usize;
        if len == 0 {
            break;
        }
        let name_start = offset + 18;
        let name_end = name_start + len;
        if name_end > slice.len() {
            break;
        }
        if len == target_bytes.len() && &slice[name_start..name_end] == target_bytes {
            let inode = u64::from_le_bytes(slice[offset..offset + 8].try_into().unwrap());
            return Some(inode);
        }
        offset = name_end;
    }
    None
}

/// Zero-allocation fast scan for finding the append offset in a directory block.
#[inline]
pub fn find_insert_offset_in_block(slice: &[u8]) -> usize {
    let mut offset = 0;
    while offset + 18 <= slice.len() {
        let len = u16::from_le_bytes([slice[offset + 16], slice[offset + 17]]) as usize;
        if len == 0 {
            break;
        }
        let name_end = offset + 18 + len;
        if name_end > slice.len() {
            break;
        }
        offset = name_end;
    }
    offset
}

/// Number of directory data blocks implied by a directory inode's `size` and first block pointer.
///
/// Legacy (v1 / pre-P3.1) directories always used exactly `blocks[0]` and stored `size == 0`,
/// so `size == 0 && first_block != 0` means one block.
#[inline]
pub fn dir_block_count(size: u64, first_block: u64, block_size: u64) -> usize {
    if size == 0 {
        usize::from(first_block != 0)
    } else {
        size.div_ceil(block_size) as usize
    }
}

/// Writes one raw entry record at `offset` (test/proof helper; mirrors `serialize_into`).
#[cfg(any(test, kani))]
fn put_raw_entry(block: &mut [u8], offset: usize, inode: u64, hash: u64, name: &[u8]) -> usize {
    block[offset..offset + 8].copy_from_slice(&inode.to_le_bytes());
    block[offset + 8..offset + 16].copy_from_slice(&hash.to_le_bytes());
    block[offset + 16..offset + 18].copy_from_slice(&(name.len() as u16).to_le_bytes());
    block[offset + 18..offset + 18 + name.len()].copy_from_slice(name);
    offset + 18 + name.len()
}

/// Property: the hash filter never causes a false negative. If an entry with the target name
/// is stored with either the correct hash or the legacy `0` hash, it is found; an entry whose
/// non-zero hash differs from a non-zero target hash is skipped.
#[cfg(any(test, kani))]
fn check_hash_filter_soundness(inode: u64, stored_hash: u64, target_hash: u64) {
    let mut block = [0u8; 48];
    put_raw_entry(&mut block, 0, inode, stored_hash, b"ab");
    let res = find_entry_in_block_with_hash(&block, "ab", target_hash);
    let filter_passes = stored_hash == 0 || target_hash == 0 || stored_hash == target_hash;
    if filter_passes {
        assert_eq!(res, Some(inode));
    } else {
        assert_eq!(res, None);
    }
    // A different name is never matched regardless of hashes.
    assert_eq!(
        find_entry_in_block_with_hash(&block, "zz", target_hash),
        None
    );
}

/// Property: the hash-filtered lookup agrees with the plain lookup whenever the target hash
/// equals the stored hash (i.e. it is a pure optimization), for a two-entry block.
#[cfg(any(test, kani))]
fn check_hash_lookup_matches_plain(h1: u64, h2: u64, pick_second: bool) {
    let mut block = [0u8; 64];
    let off = put_raw_entry(&mut block, 0, 11, h1, b"aa");
    put_raw_entry(&mut block, off, 22, h2, b"bb");
    let (name, h) = if pick_second { ("bb", h2) } else { ("aa", h1) };
    assert_eq!(
        find_entry_in_block_with_hash(&block, name, h),
        find_entry_in_block(&block, name)
    );
}

/// Property: `dir_block_count` honours the legacy rule and is the exact ceiling otherwise.
#[cfg(any(test, kani))]
fn check_dir_block_count(size: u64, first_block: u64) {
    const BS: u64 = 4096;
    let n = dir_block_count(size, first_block, BS) as u128;
    if size == 0 {
        assert_eq!(n, u128::from(first_block != 0));
    } else {
        let (size, bs) = (size as u128, BS as u128);
        assert!(n * bs >= size, "must cover every byte");
        assert!((n - 1) * bs < size, "must not over-count");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_directory_entry_serialization_roundtrip() {
        let entry = DirectoryEntry {
            inode: 42,
            hash: 1337,
            name: "hello_world.txt".to_string(),
        };

        let mut buf = Vec::new();
        entry.serialize_into(&mut buf).expect("serialize");

        let mut cursor = Cursor::new(&buf);
        let deserialized = DirectoryEntry::deserialize_from(&mut cursor)
            .expect("deserialize")
            .expect("some entry");

        assert_eq!(deserialized, entry);
    }

    #[test]
    fn test_directory_entry_reject_slash_in_name() {
        let entry = DirectoryEntry {
            inode: 1,
            hash: 0,
            name: "invalid/path.txt".to_string(),
        };

        let mut buf = Vec::new();
        let err = entry.serialize_into(&mut buf).unwrap_err();
        assert!(matches!(err, DirectoryError::Io(_)));
    }

    #[test]
    fn test_directory_entry_max_name_limit() {
        let long_name = "a".repeat(256);
        let entry = DirectoryEntry {
            inode: 1,
            hash: 0,
            name: long_name,
        };

        let mut buf = Vec::new();
        let err = entry.serialize_into(&mut buf).unwrap_err();
        assert!(matches!(err, DirectoryError::EntryTooLarge));
    }

    #[test]
    fn test_directory_iterator_multiple_entries() {
        let entries = vec![
            DirectoryEntry {
                inode: 1,
                hash: 0,
                name: "file1.txt".to_string(),
            },
            DirectoryEntry {
                inode: 2,
                hash: 0,
                name: "file2.txt".to_string(),
            },
            DirectoryEntry {
                inode: 3,
                hash: 0,
                name: "dir1".to_string(),
            },
        ];

        let mut buf = vec![0u8; 4096];
        let mut cursor = Cursor::new(&mut buf[..]);
        for entry in &entries {
            entry.serialize_into(&mut cursor).expect("serialize");
        }

        let iter = DirectoryIterator::new(&buf);
        let collected: Result<Vec<_>, _> = iter.collect();
        let collected = collected.expect("collect entries");

        assert_eq!(collected, entries);
    }

    #[test]
    fn test_directory_corrupt_utf8_name() {
        // Construct invalid UTF-8 entry manually
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u64.to_le_bytes()); // inode
        buf.extend_from_slice(&0u64.to_le_bytes()); // hash
        buf.extend_from_slice(&2u16.to_le_bytes()); // len = 2
        buf.extend_from_slice(&[0xFF, 0xFE]); // invalid UTF-8 bytes

        let mut cursor = Cursor::new(&buf);
        let err = DirectoryEntry::deserialize_from(&mut cursor).unwrap_err();
        assert!(matches!(err, DirectoryError::Utf8Error(_)));
    }

    #[test]
    fn test_hash_filename_deterministic() {
        let h1 = hash_filename("test_file.txt");
        let h2 = hash_filename("test_file.txt");
        assert_eq!(h1, h2);
        assert_ne!(h1, 0);

        let h3 = hash_filename("different_file.txt");
        assert_ne!(h1, h3);
    }

    #[test]
    fn test_find_entry_in_block_with_hash() {
        let mut block = [0u8; 128];
        let name = "hello.txt";
        let h = hash_filename(name);
        let entry = DirectoryEntry {
            inode: 12345,
            hash: h,
            name: name.to_string(),
        };
        let mut cursor = Cursor::new(&mut block[..]);
        entry.serialize_into(&mut cursor).unwrap();

        // Exact match
        assert_eq!(find_entry_in_block_with_hash(&block, name, h), Some(12345));

        // Mismatched hash
        assert_eq!(find_entry_in_block_with_hash(&block, name, h + 1), None);

        // Mismatched name
        assert_eq!(
            find_entry_in_block_with_hash(&block, "other.txt", hash_filename("other.txt")),
            None
        );

        // Legacy entry with hash == 0 should still match
        let mut legacy_block = [0u8; 128];
        let legacy_entry = DirectoryEntry {
            inode: 67890,
            hash: 0,
            name: "legacy.txt".to_string(),
        };
        let mut cursor2 = Cursor::new(&mut legacy_block[..]);
        legacy_entry.serialize_into(&mut cursor2).unwrap();

        assert_eq!(
            find_entry_in_block_with_hash(&legacy_block, "legacy.txt", hash_filename("legacy.txt")),
            Some(67890)
        );
    }

    /// Tiny deterministic xorshift PRNG so tests stay dependency-free and reproducible.
    fn xorshift(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    #[test]
    fn test_property_mirrors_hash_filter_and_block_count() {
        let edge = [0u64, 1, 2, 4095, 4096, 4097, u64::MAX - 1, u64::MAX];
        for &a in &edge {
            check_dir_block_count(a, 0);
            check_dir_block_count(a, 7);
            for &b in &edge {
                check_hash_filter_soundness(42, a, b);
                check_hash_lookup_matches_plain(a, b, false);
                check_hash_lookup_matches_plain(a, b, true);
            }
        }
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..20_000 {
            let (a, b) = (xorshift(&mut s), xorshift(&mut s));
            check_dir_block_count(a, b);
            check_hash_filter_soundness(b, a, if a & 1 == 0 { a } else { b });
            check_hash_lookup_matches_plain(a, b, a & 2 == 0);
        }
    }

    #[test]
    fn test_lookups_never_panic_on_random_corrupt_blocks() {
        let mut s = 0xDEAD_BEEF_CAFE_F00Du64;
        let mut block = [0u8; 4096];
        for round in 0..2_000 {
            for chunk in block.chunks_mut(8) {
                let v = xorshift(&mut s).to_le_bytes();
                chunk.copy_from_slice(&v[..chunk.len()]);
            }
            // Make some lengths small so scans traverse several records.
            if round % 2 == 0 {
                let mut off = 0;
                while off + 18 <= block.len() {
                    let len = (xorshift(&mut s) % 40) as u16 + 1;
                    block[off + 16..off + 18].copy_from_slice(&len.to_le_bytes());
                    off += 18 + len as usize;
                }
            }
            let _ = find_entry_in_block(&block, "ab");
            let _ = find_entry_in_block_with_hash(&block, "ab", xorshift(&mut s));
            assert!(find_insert_offset_in_block(&block) <= block.len());
        }
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    /// Prove that find_entry_in_block strictly finds a serialized entry with matching name.
    #[kani::proof]
    fn proof_find_entry_in_block_soundness() {
        let inode: u64 = kani::any();
        let hash: u64 = kani::any();
        let name_len: u16 = 4;
        let name_bytes: [u8; 4] = [b't', b'e', b's', b't'];

        let mut block = [0u8; 64];
        block[0..8].copy_from_slice(&inode.to_le_bytes());
        block[8..16].copy_from_slice(&hash.to_le_bytes());
        block[16..18].copy_from_slice(&name_len.to_le_bytes());
        block[18..22].copy_from_slice(&name_bytes);
        // Sentinel 0 at 22..24
        block[22..24].copy_from_slice(&0u16.to_le_bytes());

        let res = find_entry_in_block(&block, "test");
        assert_eq!(res, Some(inode));
    }

    /// Prove that find_entry_in_block returns None on mismatched name.
    #[kani::proof]
    fn proof_find_entry_in_block_mismatch() {
        let inode: u64 = kani::any();
        let hash: u64 = kani::any();
        let name_len: u16 = 4;
        let name_bytes: [u8; 4] = [b't', b'e', b's', b't'];

        let mut block = [0u8; 64];
        block[0..8].copy_from_slice(&inode.to_le_bytes());
        block[8..16].copy_from_slice(&hash.to_le_bytes());
        block[16..18].copy_from_slice(&name_len.to_le_bytes());
        block[18..22].copy_from_slice(&name_bytes);
        block[22..24].copy_from_slice(&0u16.to_le_bytes());

        let res = find_entry_in_block(&block, "diff");
        assert_eq!(res, None);
    }

    /// Prove that find_insert_offset_in_block correctly identifies the offset following an entry.
    #[kani::proof]
    fn proof_find_insert_offset_in_block() {
        let mut block = [0u8; 64];
        let name_len: u16 = 4;
        block[16..18].copy_from_slice(&name_len.to_le_bytes());
        // Sentinel 0 at 18 + 4 = 22
        block[22..24].copy_from_slice(&0u16.to_le_bytes());

        let offset = find_insert_offset_in_block(&block);
        assert_eq!(offset, 22);
    }

    /// Prove that find_insert_offset_in_block never exceeds slice bounds.
    #[kani::proof]
    fn proof_find_insert_offset_bounds() {
        let mut block = [0u8; 32];
        for i in 0..32 {
            block[i] = kani::any();
        }
        let offset = find_insert_offset_in_block(&block);
        assert!(offset <= block.len());
    }

    /// Memory safety on corrupt/adversarial on-disk data: neither lookup may panic or read
    /// out of bounds for ANY byte content (covers bogus `name_len` values pointing past the end).
    #[kani::proof]
    #[kani::unwind(6)]
    fn proof_lookups_never_panic_on_arbitrary_block() {
        let block: [u8; 40] = kani::any();
        let target_hash: u64 = kani::any();
        let _ = find_entry_in_block(&block, "ab");
        let _ = find_entry_in_block_with_hash(&block, "ab", target_hash);
    }

    /// The hash filter never introduces a false negative for correct or legacy (0) hashes.
    #[kani::proof]
    #[kani::unwind(4)]
    fn proof_hash_filter_soundness() {
        check_hash_filter_soundness(kani::any(), kani::any(), kani::any());
    }

    /// With matching hashes, the filtered lookup is observationally equal to the plain lookup.
    #[kani::proof]
    #[kani::unwind(4)]
    fn proof_hash_lookup_equivalent_to_plain() {
        check_hash_lookup_matches_plain(kani::any(), kani::any(), kani::any());
    }

    /// Directory sizing is legacy-compatible and an exact ceiling for every possible size.
    #[kani::proof]
    fn proof_dir_block_count() {
        check_dir_block_count(kani::any(), kani::any());
    }

    /// SipHash implementation is panic-free (no overflow / OOB) for every input up to 17 bytes,
    /// which exercises 0, 1 and 2 full 8-byte rounds plus every tail length.
    #[kani::proof]
    #[kani::unwind(18)]
    fn proof_hash_filename_panic_free() {
        let bytes: [u8; 17] = kani::any();
        let len: usize = kani::any();
        kani::assume(len <= 17);
        if let Ok(s) = core::str::from_utf8(&bytes[..len]) {
            let h1 = hash_filename(s);
            let h2 = hash_filename(s);
            assert_eq!(h1, h2, "hash must be deterministic");
        }
    }
}
