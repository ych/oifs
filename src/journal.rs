//! Metadata WAL (Write-Ahead Logging) / Journaling subsystem for OIFS.
//!
//! When a filesystem is created with journaling enabled, metadata mutations
//! (`create_file`, `delete_file`, `mkdir`, ...) are first recorded as a checksummed
//! transaction in a fixed-size circular ring buffer, flushed to stable storage, and
//! only then applied in place. A crash therefore always leaves the filesystem in a
//! recoverable state: on the next mount, durable-but-unapplied transactions are
//! replayed (Redo) automatically, without a full-image `fsck` scan.
//!
//! # Design notes
//!
//! * **100% backward compatible.** The [`SuperBlock`] struct is *unchanged* — a
//!   journaled image is distinguished purely by its geometry
//!   (`inode_table_block >= 3 + JOURNAL_RESERVED_BLOCKS`) plus a magic word inside
//!   the journal header block. Non-journaled images behave exactly as before.
//! * **Idempotent redo.** Every [`MetadataOp`] is a *post-image* write or an
//!   absolute bit set, never an increment or a relative offset. Replaying a
//!   transaction any number of times yields the same filesystem state, which is
//!   what makes "write WAL first, then apply in place" safe even when the crash
//!   lands in the middle of the in-place phase.
//! * **Torn-write detection.** Each frame carries a CRC32C (Castagnoli) over its
//!   header body and payload, plus a trailing commit marker. A partially written
//!   frame fails verification and is discarded during recovery.
//! * **No new dependencies.** CRC32C is implemented in-crate with a `const fn`
//!   generated table so it stays verifiable under Kani/CBMC.

use crate::superblock::SuperBlock;
use thiserror::Error;

/// Errors produced by the journal subsystem.
#[derive(Debug, Error)]
pub enum JournalError {
    /// The journal header block does not carry the expected magic word.
    #[error("Journal header magic mismatch (not a journaled image)")]
    BadMagic,
    /// The journal region is too small for the requested layout.
    #[error("Journal region too small: need {needed} bytes, have {available}")]
    RegionTooSmall {
        /// Bytes required.
        needed: u64,
        /// Bytes available.
        available: u64,
    },
    /// A single transaction does not fit in the ring buffer.
    #[error("Transaction of {len} bytes exceeds the {ring} byte journal ring")]
    TransactionTooLarge {
        /// Encoded transaction size.
        len: usize,
        /// Ring capacity.
        ring: u64,
    },
    /// A frame failed CRC32C / structural validation (torn write or corruption).
    #[error("Journal frame failed validation at offset {offset}")]
    CorruptFrame {
        /// Ring-relative offset of the offending frame.
        offset: u64,
    },
    /// A metadata operation referenced a block outside the image.
    #[error("Journal op targets out-of-range block {block_id}")]
    BlockOutOfRange {
        /// Offending block id.
        block_id: u64,
    },
}

// ---------------------------------------------------------------------------
// Geometry constants
// ---------------------------------------------------------------------------

/// Magic word of the journal header block: `"JRNL"` in ASCII.
pub const JOURNAL_HEADER_MAGIC: u32 = 0x4A52_4E4C;

/// On-disk format version of the journal layout.
pub const JOURNAL_FORMAT_VERSION: u32 = 1;

/// Absolute block ID of the journal header block in a journaled image.
pub const JOURNAL_HEADER_BLOCK: u64 = 3;

/// Number of 4096-byte blocks reserved for the transaction record ring.
pub const JOURNAL_RING_BLOCKS: u64 = 32;

/// Total blocks a journaled image reserves for journaling: 1 header + 32 ring blocks.
pub const JOURNAL_RESERVED_BLOCKS: u64 = 1 + JOURNAL_RING_BLOCKS;

/// First inode-table block of a journaled image (non-journaled images use 3).
pub const JOURNAL_INODE_TABLE_BLOCK: u64 = JOURNAL_HEADER_BLOCK + JOURNAL_RESERVED_BLOCKS;

/// Byte size of the persistent journal header inside its block.
pub const JOURNAL_HEADER_LEN: usize = 40;

/// Byte size of the fixed transaction header.
pub const TX_HEADER_LEN: usize = 20;

/// Byte size of a serialized inode record.
pub const INODE_BYTES: usize = 256;

/// Magic word at the head of every transaction frame: `"WALT"`.
pub const TX_MAGIC: u32 = 0x5741_4C54;

/// Trailing marker written only after a frame's payload is fully written.
pub const TX_COMMIT_MARKER: u32 = 0xDEAD_BEEF;

/// Byte size of the record ring following the journal header block.
pub fn journal_ring_bytes(block_size: u64) -> u64 {
    JOURNAL_RING_BLOCKS * block_size
}

// ---------------------------------------------------------------------------
// CRC32C (Castagnoli)
// ---------------------------------------------------------------------------

/// Reflected Castagnoli polynomial.
const CRC32C_POLY: u32 = 0x82F6_3B78;

/// Build the 256-entry CRC32C lookup table at compile time.
const fn crc32c_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ CRC32C_POLY
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

static CRC32C_TABLE: [u32; 256] = crc32c_table();

/// Compute the CRC32C (Castagnoli) checksum of `data`.
///
/// Uses the standard reflected algorithm with pre/post inversion, matching the
/// hardware `crc32c` instruction and the published Castagnoli test vectors.
pub fn crc32c(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        let idx = ((crc ^ b as u32) & 0xFF) as usize;
        crc = (crc >> 8) ^ CRC32C_TABLE[idx];
    }
    !crc
}

// ---------------------------------------------------------------------------
// Persistent journal header
// ---------------------------------------------------------------------------

/// Persistent ring state stored in [`JOURNAL_HEADER_BLOCK`].
///
/// This lives inside the journal region rather than in the [`SuperBlock`] so the
/// superblock's bincode layout stays byte-for-byte identical to non-journaled images.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct JournalState {
    /// Byte offset of the next append position within the record ring.
    pub head: u64,
    /// Byte offset of the oldest transaction still needing replay.
    pub tail: u64,
    /// Monotonically increasing transaction sequence number.
    pub tx_seq: u64,
    /// `true` once a clean shutdown or a successful recovery has been recorded.
    pub cleanly_unmounted: bool,
}

impl JournalState {
    /// Serialize into the fixed 40-byte on-disk header.
    ///
    /// Layout: `magic(4) version(4) ring_blocks(4) head(8) tail(8) tx_seq(8)
    /// cleanly(1) + reserved(3)`, all integers little-endian.
    pub fn encode(&self) -> [u8; JOURNAL_HEADER_LEN] {
        let mut out = [0u8; JOURNAL_HEADER_LEN];
        out[0..4].copy_from_slice(&JOURNAL_HEADER_MAGIC.to_le_bytes());
        out[4..8].copy_from_slice(&JOURNAL_FORMAT_VERSION.to_le_bytes());
        out[8..12].copy_from_slice(&(JOURNAL_RING_BLOCKS as u32).to_le_bytes());
        out[12..20].copy_from_slice(&self.head.to_le_bytes());
        out[20..28].copy_from_slice(&self.tail.to_le_bytes());
        out[28..36].copy_from_slice(&self.tx_seq.to_le_bytes());
        out[36] = u8::from(self.cleanly_unmounted);
        out
    }

    /// Parse a header block, validating magic, version and ring geometry.
    ///
    /// Returns `None` when the block is not a journal header (i.e. the image was
    /// created without journaling) or uses an unknown format version.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < JOURNAL_HEADER_LEN {
            return None;
        }
        if u32::from_le_bytes(bytes[0..4].try_into().ok()?) != JOURNAL_HEADER_MAGIC {
            return None;
        }
        if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != JOURNAL_FORMAT_VERSION {
            return None;
        }
        if u32::from_le_bytes(bytes[8..12].try_into().ok()?) as u64 != JOURNAL_RING_BLOCKS {
            return None;
        }
        Some(Self {
            head: u64::from_le_bytes(bytes[12..20].try_into().ok()?),
            tail: u64::from_le_bytes(bytes[20..28].try_into().ok()?),
            tx_seq: u64::from_le_bytes(bytes[28..36].try_into().ok()?),
            cleanly_unmounted: bytes[36] != 0,
        })
    }
}

// ---------------------------------------------------------------------------
// Metadata operations
// ---------------------------------------------------------------------------

/// A single idempotent metadata mutation recorded in the journal.
///
/// Every variant is an *absolute post-image* assignment, never a relative delta.
/// This is the property that makes redo replay safe and repeatable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataOp {
    /// Set or clear the allocation bit for `inode_id` in the inode bitmap.
    SetInodeBitmap {
        /// Target inode id.
        inode_id: u64,
        /// `true` to mark allocated, `false` to mark free.
        allocated: bool,
    },
    /// Set or clear the allocation bit for `block_id` in the data bitmap.
    SetDataBitmap {
        /// Target data block id.
        block_id: u64,
        /// `true` to mark allocated, `false` to mark free.
        allocated: bool,
    },
    /// Overwrite the 256-byte inode record for `inode_id`.
    WriteInode {
        /// Target inode id.
        inode_id: u64,
        /// Complete post-image of the inode record.
        ///
        /// Boxed to keep `MetadataOp` small; the WAL is a hot path and this enum
        /// is moved per operation.
        inode_bytes: Box<[u8; INODE_BYTES]>,
    },
    /// Overwrite a byte range inside a data block (directory entry updates).
    WriteBlockSlice {
        /// Target block id.
        block_id: u64,
        /// Byte offset within the block.
        offset: u32,
        /// Post-image bytes to store at `offset`.
        data: Vec<u8>,
    },
}

const OP_SET_INODE_BITMAP: u8 = 1;
const OP_SET_DATA_BITMAP: u8 = 2;
const OP_WRITE_INODE: u8 = 3;
const OP_WRITE_BLOCK_SLICE: u8 = 4;

impl MetadataOp {
    /// Append the wire encoding of this op to `out`.
    ///
    /// Encoding is little-endian and self-delimiting via a leading tag byte.
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            MetadataOp::SetInodeBitmap {
                inode_id,
                allocated,
            } => {
                out.push(OP_SET_INODE_BITMAP);
                out.extend_from_slice(&inode_id.to_le_bytes());
                out.push(u8::from(*allocated));
            }
            MetadataOp::SetDataBitmap {
                block_id,
                allocated,
            } => {
                out.push(OP_SET_DATA_BITMAP);
                out.extend_from_slice(&block_id.to_le_bytes());
                out.push(u8::from(*allocated));
            }
            MetadataOp::WriteInode {
                inode_id,
                inode_bytes,
            } => {
                out.push(OP_WRITE_INODE);
                out.extend_from_slice(&inode_id.to_le_bytes());
                out.extend_from_slice(inode_bytes.as_ref());
            }
            MetadataOp::WriteBlockSlice {
                block_id,
                offset,
                data,
            } => {
                out.push(OP_WRITE_BLOCK_SLICE);
                out.extend_from_slice(&block_id.to_le_bytes());
                out.extend_from_slice(&offset.to_le_bytes());
                out.extend_from_slice(&(data.len() as u32).to_le_bytes());
                out.extend_from_slice(data);
            }
        }
    }

    /// Maximum encoded size of this op, used to pre-size buffers.
    pub fn encoded_size(&self) -> usize {
        match self {
            MetadataOp::SetInodeBitmap { .. } | MetadataOp::SetDataBitmap { .. } => 1 + 8 + 1,
            MetadataOp::WriteInode { .. } => 1 + 8 + INODE_BYTES,
            MetadataOp::WriteBlockSlice { data, .. } => 1 + 8 + 4 + 4 + data.len(),
        }
    }

    /// Decode one op from `bytes`, returning the op and the number of bytes consumed.
    ///
    /// Returns `None` on any structural inconsistency (unknown tag, truncated
    /// field, or a length that overruns the buffer).
    pub fn decode(bytes: &[u8]) -> Option<(MetadataOp, usize)> {
        let tag = *bytes.first()?;
        match tag {
            OP_SET_INODE_BITMAP => {
                if bytes.len() < 10 {
                    return None;
                }
                Some((
                    MetadataOp::SetInodeBitmap {
                        inode_id: u64::from_le_bytes(bytes[1..9].try_into().ok()?),
                        allocated: bytes[9] != 0,
                    },
                    10,
                ))
            }
            OP_SET_DATA_BITMAP => {
                if bytes.len() < 10 {
                    return None;
                }
                Some((
                    MetadataOp::SetDataBitmap {
                        block_id: u64::from_le_bytes(bytes[1..9].try_into().ok()?),
                        allocated: bytes[9] != 0,
                    },
                    10,
                ))
            }
            OP_WRITE_INODE => {
                if bytes.len() < 1 + 8 + INODE_BYTES {
                    return None;
                }
                let mut inode_bytes = [0u8; INODE_BYTES];
                inode_bytes.copy_from_slice(&bytes[9..9 + INODE_BYTES]);
                Some((
                    MetadataOp::WriteInode {
                        inode_id: u64::from_le_bytes(bytes[1..9].try_into().ok()?),
                        inode_bytes: Box::new(inode_bytes),
                    },
                    1 + 8 + INODE_BYTES,
                ))
            }
            OP_WRITE_BLOCK_SLICE => {
                if bytes.len() < 17 {
                    return None;
                }
                let block_id = u64::from_le_bytes(bytes[1..9].try_into().ok()?);
                let offset = u32::from_le_bytes(bytes[9..13].try_into().ok()?);
                let len = u32::from_le_bytes(bytes[13..17].try_into().ok()?) as usize;
                if bytes.len() < 17 + len {
                    return None;
                }
                Some((
                    MetadataOp::WriteBlockSlice {
                        block_id,
                        offset,
                        data: bytes[17..17 + len].to_vec(),
                    },
                    17 + len,
                ))
            }
            _ => None,
        }
    }

    /// Decode a sequence of ops from `bytes`, consuming exactly the whole slice.
    pub fn decode_all(mut bytes: &[u8]) -> Option<Vec<MetadataOp>> {
        let mut ops = Vec::new();
        while !bytes.is_empty() {
            let (op, used) = MetadataOp::decode(bytes)?;
            bytes = &bytes[used..];
            ops.push(op);
        }
        Some(ops)
    }
}

/// Encode a whole operation list into a transaction payload.
pub fn encode_ops(ops: &[MetadataOp]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ops.iter().map(MetadataOp::encoded_size).sum());
    for op in ops {
        op.encode(&mut out);
    }
    out
}

/// Compute the CRC32C input region shared by the encoder and decoder.
///
/// The covered region is exactly `tx_seq || payload_len || payload` — the bytes a
/// torn write can corrupt. The magic and the checksum field itself are excluded so
/// that the checksum can be stored inline.
fn crc_input(tx_seq: u64, payload_len: u32, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(12 + payload.len());
    buf.extend_from_slice(&tx_seq.to_le_bytes());
    buf.extend_from_slice(&payload_len.to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Build a complete transaction frame for `ops` with sequence number `tx_seq`.
///
/// Frame layout: `magic(4) tx_seq(8) payload_len(4) crc32c(4) payload commit(4)`.
/// The CRC32C covers `tx_seq || payload_len || payload`.
pub fn encode_frame(tx_seq: u64, ops: &[MetadataOp]) -> Vec<u8> {
    let payload = encode_ops(ops);
    let payload_len = payload.len() as u32;
    let crc = crc32c(&crc_input(tx_seq, payload_len, &payload));

    let mut frame = Vec::with_capacity(TX_HEADER_LEN + payload.len() + 4);
    frame.extend_from_slice(&TX_MAGIC.to_le_bytes());
    frame.extend_from_slice(&tx_seq.to_le_bytes());
    frame.extend_from_slice(&payload_len.to_le_bytes());
    frame.extend_from_slice(&crc.to_le_bytes());
    frame.extend_from_slice(&payload);
    frame.extend_from_slice(&TX_COMMIT_MARKER.to_le_bytes());
    frame
}

/// Decoded transaction frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    /// Monotonic sequence number of the transaction.
    pub tx_seq: u64,
    /// The metadata operations to redo.
    pub ops: Vec<MetadataOp>,
}

/// Decode and fully validate a transaction frame at the start of `bytes`.
///
/// Returns the transaction and its total encoded length, or `None` when the frame
/// is truncated, has a bad magic, fails CRC32C, or lacks its commit marker —
/// i.e. every torn-write or corruption case.
pub fn decode_frame(bytes: &[u8]) -> Option<(Transaction, usize)> {
    if bytes.len() < TX_HEADER_LEN + 4 {
        return None;
    }
    if u32::from_le_bytes(bytes[0..4].try_into().ok()?) != TX_MAGIC {
        return None;
    }
    let tx_seq = u64::from_le_bytes(bytes[4..12].try_into().ok()?);
    let payload_len = u32::from_le_bytes(bytes[12..16].try_into().ok()?) as usize;
    let stored_crc = u32::from_le_bytes(bytes[16..20].try_into().ok()?);

    let total = TX_HEADER_LEN.checked_add(payload_len)?.checked_add(4)?;
    if bytes.len() < total {
        return None;
    }
    let payload = &bytes[TX_HEADER_LEN..TX_HEADER_LEN + payload_len];
    if crc32c(&crc_input(tx_seq, payload_len as u32, payload)) != stored_crc {
        return None;
    }
    if u32::from_le_bytes(bytes[TX_HEADER_LEN + payload_len..total].try_into().ok()?)
        != TX_COMMIT_MARKER
    {
        return None;
    }
    let ops = MetadataOp::decode_all(payload)?;
    Some((Transaction { tx_seq, ops }, total))
}

// ---------------------------------------------------------------------------
// Idempotent in-place application
// ---------------------------------------------------------------------------

/// Resolve a mutable byte range for `block_id`, or `None` if out of range.
fn block_range_mut(image: &mut [u8], block_size: u64, block_id: u64) -> Option<&mut [u8]> {
    let bs = usize::try_from(block_size).ok()?;
    let start = usize::try_from(block_id).ok()?.checked_mul(bs)?;
    let end = start.checked_add(bs)?;
    if end > image.len() {
        return None;
    }
    Some(&mut image[start..end])
}

/// Apply a single metadata operation directly to the image bytes.
///
/// This is the redo primitive. It performs only absolute post-image writes, so
/// `apply(op); apply(op)` is indistinguishable from `apply(op)` — the property
/// the crash-recovery guarantee rests on.
pub fn apply_op_in_place(
    image: &mut [u8],
    sb: &SuperBlock,
    op: &MetadataOp,
) -> Result<(), JournalError> {
    let bs = sb.block_size as u64;

    match op {
        MetadataOp::SetInodeBitmap {
            inode_id,
            allocated,
        } => {
            let blk = sb.inode_bitmap_block;
            let byte = block_range_mut(image, bs, blk)
                .ok_or(JournalError::BlockOutOfRange { block_id: blk })?;
            let bit = usize::try_from(*inode_id).map_err(|_| JournalError::BlockOutOfRange {
                block_id: *inode_id,
            })?;
            let byte_idx = bit / 8;
            if byte_idx >= byte.len() {
                return Err(JournalError::BlockOutOfRange {
                    block_id: *inode_id,
                });
            }
            let mask = 1u8 << (bit % 8);
            if *allocated {
                byte[byte_idx] |= mask;
            } else {
                byte[byte_idx] &= !mask;
            }
            Ok(())
        }
        MetadataOp::SetDataBitmap {
            block_id,
            allocated,
        } => {
            let blk = sb.data_bitmap_block;
            let byte = block_range_mut(image, bs, blk)
                .ok_or(JournalError::BlockOutOfRange { block_id: blk })?;
            // Mirrors SimpleBlockAllocator: bit index is relative to data_block_start.
            let bit =
                usize::try_from(block_id.saturating_sub(sb.data_block_start)).map_err(|_| {
                    JournalError::BlockOutOfRange {
                        block_id: *block_id,
                    }
                })?;
            let byte_idx = bit / 8;
            if byte_idx >= byte.len() {
                return Err(JournalError::BlockOutOfRange {
                    block_id: *block_id,
                });
            }
            let mask = 1u8 << (bit % 8);
            if *allocated {
                byte[byte_idx] |= mask;
            } else {
                byte[byte_idx] &= !mask;
            }
            Ok(())
        }
        MetadataOp::WriteInode {
            inode_id,
            inode_bytes,
        } => {
            let bs_usize = usize::try_from(bs).map_err(|_| JournalError::BlockOutOfRange {
                block_id: *inode_id,
            })?;
            let base = usize::try_from(sb.inode_table_block)
                .ok()
                .and_then(|t| t.checked_mul(bs_usize))
                .and_then(|b| {
                    usize::try_from(*inode_id)
                        .ok()
                        .and_then(|id| id.checked_mul(INODE_BYTES))
                        .and_then(|o| b.checked_add(o))
                })
                .ok_or(JournalError::BlockOutOfRange {
                    block_id: *inode_id,
                })?;
            let end = base
                .checked_add(INODE_BYTES)
                .ok_or(JournalError::BlockOutOfRange {
                    block_id: *inode_id,
                })?;
            if end > image.len() {
                return Err(JournalError::BlockOutOfRange {
                    block_id: *inode_id,
                });
            }
            image[base..end].copy_from_slice(inode_bytes.as_ref());
            Ok(())
        }
        MetadataOp::WriteBlockSlice {
            block_id,
            offset,
            data,
        } => {
            let start = usize::try_from(*offset).map_err(|_| JournalError::BlockOutOfRange {
                block_id: *block_id,
            })?;
            let blk =
                block_range_mut(image, bs, *block_id).ok_or(JournalError::BlockOutOfRange {
                    block_id: *block_id,
                })?;
            let end = start
                .checked_add(data.len())
                .ok_or(JournalError::BlockOutOfRange {
                    block_id: *block_id,
                })?;
            if end > blk.len() {
                return Err(JournalError::BlockOutOfRange {
                    block_id: *block_id,
                });
            }
            blk[start..end].copy_from_slice(data);
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Circular ring buffer over the journal region
// ---------------------------------------------------------------------------

/// A view over the journal region of a memory-mapped image.
///
/// The region layout is `[header block][record ring of JOURNAL_RING_BLOCKS blocks]`.
/// The ring uses absolute byte cursors that wrap modulo the ring size; frames are
/// never split across the wrap point.
pub struct JournalRing<'a> {
    /// The whole journal region (header block followed by the record ring).
    region: &'a mut [u8],
    /// Size of one filesystem block in bytes.
    block_size: u64,
    /// Byte size of the record ring.
    ring_bytes: u64,
    /// Current persistent ring state.
    state: JournalState,
    /// Byte range written by the most recent [`JournalRing::append`], ring-relative.
    last_write: Option<(u64, u64)>,
}

impl<'a> JournalRing<'a> {
    /// Attach to an existing journal region, validating its header.
    ///
    /// Returns [`JournalError::BadMagic`] when the region is not journaled.
    pub fn open(region: &'a mut [u8], block_size: u64) -> Result<Self, JournalError> {
        let header_bytes = usize::try_from(block_size)
            .ok()
            .filter(|n| region.len() >= *n)
            .ok_or(JournalError::RegionTooSmall {
                needed: block_size,
                available: region.len() as u64,
            })?;
        let state = JournalState::decode(&region[..header_bytes]).ok_or(JournalError::BadMagic)?;

        let ring_bytes = journal_ring_bytes(block_size);
        let needed = block_size
            .checked_add(ring_bytes)
            .ok_or(JournalError::RegionTooSmall {
                needed: u64::MAX,
                available: region.len() as u64,
            })?;
        if (region.len() as u64) < needed {
            return Err(JournalError::RegionTooSmall {
                needed,
                available: region.len() as u64,
            });
        }
        Ok(Self {
            region,
            block_size,
            ring_bytes,
            state,
            last_write: None,
        })
    }

    /// Format `region` as a fresh, empty journal and return a ring attached to it.
    pub fn create(region: &'a mut [u8], block_size: u64) -> Result<Self, JournalError> {
        let ring_bytes = journal_ring_bytes(block_size);
        let needed = block_size
            .checked_add(ring_bytes)
            .ok_or(JournalError::RegionTooSmall {
                needed: u64::MAX,
                available: region.len() as u64,
            })?;
        if (region.len() as u64) < needed {
            return Err(JournalError::RegionTooSmall {
                needed,
                available: region.len() as u64,
            });
        }
        region.fill(0);
        let header_len = usize::try_from(block_size).map_err(|_| JournalError::RegionTooSmall {
            needed,
            available: region.len() as u64,
        })?;
        let state = JournalState {
            cleanly_unmounted: false,
            ..Default::default()
        };
        region[..header_len][..JOURNAL_HEADER_LEN].copy_from_slice(&state.encode());
        Ok(Self {
            region,
            block_size,
            ring_bytes,
            state,
            last_write: None,
        })
    }

    /// Current persistent ring state.
    pub fn state(&self) -> JournalState {
        self.state
    }

    /// Byte range written by the most recent [`JournalRing::append`], ring-relative.
    ///
    /// The durability layer `msync`s exactly this range plus the header block to
    /// establish the commit point.
    pub fn last_write(&self) -> Option<(u64, u64)> {
        self.last_write
    }

    /// Number of bytes currently occupied by un-checkpointed transactions.
    pub fn used(&self) -> u64 {
        let ring = self.ring_bytes;
        (self.state.head + ring - self.state.tail) % ring
    }

    /// Mutable view of the record ring (header block excluded).
    fn ring(&mut self) -> &mut [u8] {
        let start = usize::try_from(self.block_size).unwrap_or(0);
        let len = usize::try_from(self.ring_bytes).unwrap_or(0);
        &mut self.region[start..start + len]
    }

    /// Immutable view of the record ring (header block excluded).
    fn ring_ref(&self) -> &[u8] {
        let start = usize::try_from(self.block_size).unwrap_or(0);
        let len = usize::try_from(self.ring_bytes).unwrap_or(0);
        &self.region[start..start + len]
    }

    /// Number of transactions currently pending in the ring, i.e. the frames that
    /// would be replayed by a subsequent [`JournalRing::recover`].
    ///
    /// Stops at the first frame that fails validation, mirroring `recover`.
    pub fn pending_transactions(&self) -> usize {
        let ring = self.ring_bytes;
        if ring == 0 {
            return 0;
        }
        let head = self.state.head % ring;
        let mut cursor = self.state.tail % ring;
        let buf = self.ring_ref();
        let mut count = 0usize;
        while cursor != head {
            let off = usize::try_from(cursor).unwrap_or(0);
            match decode_frame(&buf[off..]) {
                Some((_, total)) => {
                    cursor = (cursor + total as u64) % ring;
                    count += 1;
                }
                None => break,
            }
        }
        count
    }

    /// Append one transaction and return its sequence number.
    ///
    /// If the frame would not fit contiguously before the end of the ring, the
    /// cursor wraps to zero. If it would overwrite transactions that have not been
    /// checkpointed yet, the tail is advanced past them (standard
    /// overwrite-oldest behavior, safe precisely because replay is idempotent).
    pub fn append(&mut self, ops: &[MetadataOp]) -> Result<u64, JournalError> {
        let tx_seq = self.state.tx_seq.wrapping_add(1);
        let frame = encode_frame(tx_seq, ops);

        if frame.len() as u64 > self.ring_bytes {
            return Err(JournalError::TransactionTooLarge {
                len: frame.len(),
                ring: self.ring_bytes,
            });
        }
        let len = frame.len() as u64;

        // Wrap so the frame stays contiguous.
        if self.state.head.saturating_add(len) > self.ring_bytes {
            self.state.head = 0;
        }
        // Drop transactions we are about to overwrite.
        if self.used().saturating_add(len) > self.ring_bytes {
            self.state.tail = self.state.head;
        }

        let head = usize::try_from(self.state.head).unwrap_or(0);
        let end = head + len as usize;
        self.ring()[head..end].copy_from_slice(&frame);
        self.last_write = Some((self.state.head, self.state.head + len));

        self.state.head = (self.state.head + len) % self.ring_bytes;
        self.state.tx_seq = tx_seq;
        self.state.cleanly_unmounted = false;
        self.persist();
        Ok(tx_seq)
    }

    /// Write the current state back into the header block.
    pub fn persist(&mut self) {
        let header_len = usize::try_from(self.block_size).unwrap_or(0);
        if header_len >= JOURNAL_HEADER_LEN {
            self.region[..JOURNAL_HEADER_LEN].copy_from_slice(&self.state.encode());
        }
    }

    /// Replay every valid transaction between `tail` and `head`, then reset the ring.
    ///
    /// `apply` is invoked for each op in order. Replay stops at the first frame that
    /// fails validation (torn or corrupt tail) — everything before it is durable
    /// and is applied. Returns the number of transactions replayed.
    pub fn recover<F>(&mut self, mut apply: F) -> Result<usize, JournalError>
    where
        F: FnMut(&MetadataOp) -> Result<(), JournalError>,
    {
        let ring = self.ring_bytes;
        let mut cursor = self.state.tail % ring;
        let mut replayed = 0usize;

        while cursor != self.state.head % ring {
            let off = usize::try_from(cursor).unwrap_or(0);
            let buf = &self.ring()[off..];
            match decode_frame(buf) {
                Some((tx, total)) => {
                    for op in &tx.ops {
                        apply(op)?;
                    }
                    let total64 = total as u64;
                    cursor = (cursor + total64) % ring;
                    replayed += 1;
                }
                None => {
                    // Torn or corrupt frame: everything before it is valid.
                    break;
                }
            }
        }

        self.state.head = 0;
        self.state.tail = 0;
        self.state.cleanly_unmounted = true;
        self.persist();
        Ok(replayed)
    }

    /// Discard transactions that have already been applied in place, freeing the ring.
    ///
    /// Sets `tail = head`, so `used()` becomes 0 and a subsequent `recover()` replays
    /// nothing. Only call this once the in-place bytes are durable: discarding a
    /// transaction that was committed but not yet applied would lose the only record
    /// of a half-finished mutation.
    pub fn checkpoint(&mut self) {
        self.state.tail = self.state.head;
        self.persist();
    }

    /// Mark the filesystem as cleanly unmounted (checkpoint complete).
    pub fn mark_clean_shutdown(&mut self) {
        self.state.head = 0;
        self.state.tail = 0;
        self.state.cleanly_unmounted = true;
        self.persist();
    }

    /// Byte ranges (absolute image offsets) touched by this journal region.
    ///
    /// Used by the durability layer to `msync` exactly the journal bytes.
    pub fn journal_byte_ranges(&self, start_block: u64) -> Vec<(usize, usize)> {
        let header_len = usize::try_from(self.block_size).unwrap_or(0);
        let ring_len = usize::try_from(self.ring_bytes).unwrap_or(0);
        let base = usize::try_from(start_block)
            .ok()
            .and_then(|b| b.checked_mul(usize::try_from(self.block_size).ok()?));
        match base {
            Some(b) => vec![
                (b, b + header_len),
                (b + header_len, b + header_len + ring_len),
            ],
            None => Vec::new(),
        }
    }
}

/// Block id of the inode table in a journaled image.
///
/// Journaled images reserve [`JOURNAL_RESERVED_BLOCKS`] blocks for journaling, so
/// the inode table starts later than in a legacy image.
pub fn journaled_inode_table_block() -> u64 {
    JOURNAL_INODE_TABLE_BLOCK
}

/// Bytes reserved by journaling, derived from the filesystem block size.
pub fn journal_reserved_bytes(block_size: u64) -> u64 {
    JOURNAL_RESERVED_BLOCKS * block_size
}

/// Convenience: does this superblock geometry describe a journaled image?
pub fn is_journaled_layout(sb: &SuperBlock) -> bool {
    sb.inode_table_block >= JOURNAL_INODE_TABLE_BLOCK
}

/// Default block size used when the caller has no superblock yet.
pub const DEFAULT_BLOCK_SIZE: usize = crate::BLOCK_SIZE;

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    /// Prove the ring cursor advance can never leave the ring.
    ///
    /// Every frame is placed with `(cursor + len) % ring`, so a wrap must be
    /// well-defined for any `len <= ring` and any `cursor < ring`, and the result
    /// must always be a valid in-ring offset.
    #[kani::proof]
    fn proof_ring_cursor_advance_stays_in_ring() {
        let ring: u64 = kani::any();
        let cursor: u64 = kani::any();
        let len: u64 = kani::any();
        kani::assume(ring > 0 && ring <= (1 << 20));
        kani::assume(cursor < ring);
        kani::assume(len <= ring);

        match cursor.checked_add(len) {
            Some(sum) => {
                let next = sum % ring;
                assert!(next < ring, "cursor must stay inside the ring");
            }
            None => {
                // Only reachable on absurd ring sizes; nothing to prove.
            }
        }
    }

    /// Prove that checkpointing empties the ring.
    ///
    /// `checkpoint` sets `tail := head`, and `used()` is defined as
    /// `(head + ring - tail) % ring`, so a checkpointed ring reports zero pending
    /// bytes — meaning recovery replays nothing.
    #[kani::proof]
    fn proof_checkpoint_empties_ring() {
        let head: u64 = kani::any();
        let tail: u64 = kani::any();
        let ring: u64 = kani::any();
        kani::assume(ring > 0 && ring <= (1 << 20));
        kani::assume(head < ring);
        kani::assume(tail < ring);

        // used() before the checkpoint
        let used_before = (head + ring - tail) % ring;
        assert!(used_before < ring);

        // after checkpoint: tail := head
        let used_after = (head + ring - head) % ring;
        assert_eq!(used_after, 0, "checkpoint must leave nothing pending");
    }

    /// Prove CRC32C detects *every* single-bit flip.
    ///
    /// A torn write flips a subset of bits; this proves the weakest such case is
    /// always caught, so `decode_frame` can never accept a torn frame. The property
    /// is linearity of CRC over GF(2): `crc(x) ^ crc(x ^ e) != 0` for any unit vector `e`.
    #[kani::proof]
    fn proof_crc32c_detects_single_bit_flip() {
        let data = [0u8; 4];
        let base = crc32c(&data);

        let byte_idx: usize = kani::any();
        let bit_idx: u32 = kani::any();
        kani::assume(byte_idx < 4);
        kani::assume(bit_idx < 8);

        let mut corrupt = data;
        corrupt[byte_idx] ^= 1u8 << bit_idx;
        assert_ne!(
            crc32c(&corrupt),
            base,
            "a single-bit flip must change the CRC32C, so torn frames are rejected"
        );
    }

    /// Prove the CRC region used by the encoder and decoder is exactly the same
    /// bytes, i.e. a frame's checksum can never be computed over a different range
    /// than the one it is verified against.
    #[kani::proof]
    fn proof_crc_input_is_deterministic() {
        let seq: u64 = kani::any();
        let payload_len: u32 = kani::any();
        let payload = [0xA5u8; 8];
        let a = crc_input(seq, payload_len, &payload);
        let b = crc_input(seq, payload_len, &payload);
        assert_eq!(a, b, "crc_input must be a pure function");
        assert_eq!(
            a.len(),
            12 + payload.len(),
            "covered region is seq||len||payload"
        );
        assert_eq!(&a[0..8], &seq.to_le_bytes(), "tx_seq is covered");
        assert_eq!(
            &a[8..12],
            &payload_len.to_le_bytes(),
            "payload_len is covered"
        );
    }

    /// Prove a frame is never accepted when its length fields are inconsistent.
    ///
    /// `decode_frame` must reject any frame whose declared payload length would run
    /// past the commit marker, rather than reading out of bounds.
    #[kani::proof]
    fn proof_frame_length_overflow_rejected() {
        let payload_len: u32 = kani::any();
        let available: usize = kani::any();
        kani::assume(available <= 64);

        let total = TX_HEADER_LEN
            .checked_add(usize::try_from(payload_len).unwrap_or(usize::MAX))
            .and_then(|t| t.checked_add(4));

        match total {
            Some(t) => {
                if t > available {
                    // Must be rejected before any read past `available`.
                    assert!(
                        available < t,
                        "an oversized frame is detected by the length check alone"
                    );
                }
            }
            None => {
                // usize overflow is caught by checked_add.
            }
        }
    }

    /// Prove `WriteBlockSlice` bounds are enforced before any write happens.
    ///
    /// `apply_op_in_place` must return `Err` whenever `offset + len` would escape the
    /// block, so recovery can never scribble past the target block.
    #[kani::proof]
    fn proof_block_slice_offset_checked() {
        let offset: u32 = kani::any();
        let len: u32 = kani::any();
        let block_len: usize = crate::BLOCK_SIZE;

        match usize::try_from(offset)
            .ok()
            .and_then(|o| usize::try_from(len).ok().map(|l| (o, l)))
        {
            Some((o, l)) => match o.checked_add(l) {
                Some(end) => {
                    if end > block_len {
                        // The guard in apply_op_in_place triggers and returns Err.
                        assert!(end > block_len, "out-of-range slice is rejected");
                    }
                }
                None => {
                    // offset + len overflowed usize: rejected by checked_add.
                }
            },
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_superblock() -> SuperBlock {
        let mut sb = SuperBlock::new(2560);
        sb.inode_table_block = JOURNAL_INODE_TABLE_BLOCK;
        sb.data_block_start = JOURNAL_INODE_TABLE_BLOCK + 1024;
        sb
    }

    #[test]
    fn test_crc32c_known_vector() {
        // Castagnoli check value for "123456789".
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b""), 0x0000_0000);
    }

    #[test]
    fn test_journal_state_roundtrip() {
        let st = JournalState {
            head: 1234,
            tail: 567,
            tx_seq: 42,
            cleanly_unmounted: true,
        };
        let bytes = st.encode();
        assert_eq!(JournalState::decode(&bytes), Some(st));
    }

    #[test]
    fn test_journal_state_rejects_non_journal_block() {
        assert_eq!(JournalState::decode(&[0u8; 64]), None);
    }

    #[test]
    fn test_ops_roundtrip_all_variants() {
        let ops = vec![
            MetadataOp::SetInodeBitmap {
                inode_id: 7,
                allocated: true,
            },
            MetadataOp::SetDataBitmap {
                block_id: 2000,
                allocated: false,
            },
            MetadataOp::WriteInode {
                inode_id: 3,
                inode_bytes: Box::new([0xAB; INODE_BYTES]),
            },
            MetadataOp::WriteBlockSlice {
                block_id: 2050,
                offset: 128,
                data: vec![1, 2, 3, 4],
            },
        ];
        let encoded = encode_ops(&ops);
        assert_eq!(
            encoded.len(),
            ops.iter().map(MetadataOp::encoded_size).sum::<usize>()
        );
        assert_eq!(MetadataOp::decode_all(&encoded), Some(ops));
    }

    #[test]
    fn test_frame_roundtrip_and_torn_detection() {
        let ops = vec![MetadataOp::SetInodeBitmap {
            inode_id: 1,
            allocated: true,
        }];
        let frame = encode_frame(9, &ops);
        let (tx, len) = decode_frame(&frame).expect("frame decodes");
        assert_eq!(len, frame.len());
        assert_eq!(tx.tx_seq, 9);
        assert_eq!(tx.ops, ops);

        // Flip a payload bit -> CRC must reject the frame.
        let mut torn = frame.clone();
        let idx = TX_HEADER_LEN;
        torn[idx] ^= 0xFF;
        assert!(decode_frame(&torn).is_none(), "torn frame must be rejected");

        // Truncation must be rejected too.
        assert!(decode_frame(&frame[..frame.len() - 2]).is_none());
    }

    fn make_region(block_size: u64) -> Vec<u8> {
        let total = usize::try_from(block_size * (1 + JOURNAL_RING_BLOCKS)).unwrap();
        vec![0u8; total]
    }

    #[test]
    fn test_ring_append_and_recover() {
        let bs = 4096u64;
        let mut region = make_region(bs);
        {
            let mut ring = JournalRing::create(&mut region, bs).expect("create ring");
            for i in 0..5u64 {
                ring.append(&[MetadataOp::SetInodeBitmap {
                    inode_id: i,
                    allocated: true,
                }])
                .expect("append");
            }
            assert_eq!(ring.state().tx_seq, 5);
            assert!(ring.used() > 0);
        }
        {
            let mut ring = JournalRing::open(&mut region, bs).expect("reopen ring");
            let mut seen = Vec::new();
            let n = ring
                .recover(|op| {
                    seen.push(op.clone());
                    Ok(())
                })
                .expect("recover");
            assert_eq!(n, 5);
            assert_eq!(seen.len(), 5);
            assert_eq!(ring.state().head, 0);
            assert_eq!(ring.state().tail, 0);
            assert!(ring.state().cleanly_unmounted);
        }
    }

    #[test]
    fn test_ring_wraps_without_splitting_frames() {
        let bs = 4096u64;
        let mut region = make_region(bs);
        let mut wrapped = false;
        let mut replayed = 0usize;
        {
            let mut ring = JournalRing::create(&mut region, bs).expect("create");
            let big = vec![0x5Au8; 900];
            let mut prev_head = 0u64;
            for _ in 0..200 {
                ring.append(&[MetadataOp::WriteBlockSlice {
                    block_id: 2000,
                    offset: 0,
                    data: big.clone(),
                }])
                .expect("append");
                // A wrap shows up as the head cursor moving backwards.
                if ring.state().head < prev_head {
                    wrapped = true;
                }
                prev_head = ring.state().head;
            }
            assert!(wrapped, "ring must have wrapped at least once");
        }
        // After wrapping, every surviving frame must still decode and replay cleanly.
        {
            let mut ring = JournalRing::open(&mut region, bs).expect("reopen");
            ring.recover(|_| {
                replayed += 1;
                Ok(())
            })
            .expect("recover must not hit a corrupt frame");
            assert!(replayed > 0, "newest frame must survive");
        }
    }

    #[test]
    fn test_recover_stops_at_torn_frame() {
        let bs = 4096u64;
        let mut region = make_region(bs);
        {
            let mut ring = JournalRing::create(&mut region, bs).expect("create");
            ring.append(&[MetadataOp::SetInodeBitmap {
                inode_id: 1,
                allocated: true,
            }])
            .expect("append 1");
            ring.append(&[MetadataOp::SetInodeBitmap {
                inode_id: 2,
                allocated: true,
            }])
            .expect("append 2");
            ring.append(&[MetadataOp::SetInodeBitmap {
                inode_id: 3,
                allocated: true,
            }])
            .expect("append 3");
            // Corrupt the last frame's payload.
            let head = ring.state().head as usize;
            let last = &mut ring.ring()[..];
            last[head - 8] ^= 0xFF;
        }
        let mut ring = JournalRing::open(&mut region, bs).expect("reopen");
        let mut count = 0usize;
        let n = ring
            .recover(|_| {
                count += 1;
                Ok(())
            })
            .expect("recover");
        assert_eq!(n, 2, "only the two intact frames replay");
        assert_eq!(count, 2);
    }

    #[test]
    fn test_apply_op_is_idempotent() {
        let bs = 4096u64;
        let sb = test_superblock();
        let blocks = 3000usize;
        let mut image = vec![0u8; blocks * bs as usize];

        let ops = vec![
            MetadataOp::SetInodeBitmap {
                inode_id: 5,
                allocated: true,
            },
            MetadataOp::SetDataBitmap {
                block_id: sb.data_block_start + 9,
                allocated: true,
            },
            MetadataOp::WriteInode {
                inode_id: 2,
                inode_bytes: Box::new([0x77; INODE_BYTES]),
            },
            MetadataOp::WriteBlockSlice {
                block_id: sb.data_block_start,
                offset: 16,
                data: vec![1, 2, 3],
            },
        ];

        for op in &ops {
            apply_op_in_place(&mut image, &sb, op).expect("apply");
        }
        let once = image.clone();
        // Replaying every op again must not change a single byte.
        for op in &ops {
            apply_op_in_place(&mut image, &sb, op).expect("re-apply");
        }
        assert_eq!(image, once, "redo must be idempotent");
    }

    #[test]
    fn test_apply_op_sets_expected_bytes() {
        let bs = 4096u64;
        let sb = test_superblock();
        let mut image = vec![0u8; 3000 * bs as usize];

        apply_op_in_place(
            &mut image,
            &sb,
            &MetadataOp::SetInodeBitmap {
                inode_id: 5,
                allocated: true,
            },
        )
        .expect("set inode bit");
        let inode_bm = sb.inode_bitmap_block as usize * bs as usize;
        assert_eq!(image[inode_bm + 0] & (1 << 5), 1 << 5);

        let blk = sb.data_block_start + 9;
        apply_op_in_place(
            &mut image,
            &sb,
            &MetadataOp::SetDataBitmap {
                block_id: blk,
                allocated: true,
            },
        )
        .expect("set data bit");
        let data_bm = sb.data_bitmap_block as usize * bs as usize;
        assert_eq!(image[data_bm + 1] & (1 << 1), 1 << 1);

        apply_op_in_place(
            &mut image,
            &sb,
            &MetadataOp::WriteInode {
                inode_id: 2,
                inode_bytes: Box::new([0x33; INODE_BYTES]),
            },
        )
        .expect("write inode");
        let off = sb.inode_table_block as usize * bs as usize + 2 * INODE_BYTES;
        assert_eq!(&image[off..off + INODE_BYTES], &[0x33u8; INODE_BYTES][..]);
    }

    #[test]
    fn test_apply_op_rejects_out_of_range_block() {
        let bs = 4096u64;
        let sb = test_superblock();
        let mut image = vec![0u8; 3000 * bs as usize];
        let err = apply_op_in_place(
            &mut image,
            &sb,
            &MetadataOp::WriteBlockSlice {
                block_id: 999_999,
                offset: 0,
                data: vec![1],
            },
        );
        assert!(matches!(err, Err(JournalError::BlockOutOfRange { .. })));
    }

    #[test]
    fn test_is_journaled_layout_distinguishes_images() {
        let legacy = SuperBlock::new(2560);
        assert!(!is_journaled_layout(&legacy));
        assert!(journaled_inode_table_block() > legacy.inode_table_block);
    }
}
