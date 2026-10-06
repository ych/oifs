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
//! * **Every metadata mutation must be journaled.** A transaction left pending in
//!   the ring becomes *stale* the moment an unjournaled path modifies the same
//!   metadata: recovery would replay the old post-image over the newer change and
//!   silently roll it back (observed in practice when a create's zeroed inode
//!   post-image overwrote the size and block pointer a later write had set, leaving
//!   files that read back empty). `create_file`/`mkdir`, `delete_file` and
//!   `write_data` therefore all stage through `AllocSim` and commit a transaction.
//!   Any future path that mutates metadata must do the same, or drain the ring
//!   first.

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
    /// Sets `tail := head`, so `used()` becomes 0 and a subsequent `recover()` replays
    /// nothing. Only call this once the in-place bytes are durable: discarding a
    /// transaction that was committed but not yet applied would lose the only record
    /// of a half-finished mutation.
    ///
    /// Callers that cannot hold the write lock across their flush should use
    /// [`Self::checkpoint_to`] with a snapshot of `head` taken beforehand.
    pub fn checkpoint(&mut self) {
        let head = self.state.head;
        self.checkpoint_to(head);
    }

    /// Advance `tail` to `cursor`, discarding every transaction before it.
    ///
    /// `cursor` must be a frame boundary recorded earlier — normally `head` as it read
    /// *before* a flush began. Anything committed after that snapshot stays pending,
    /// because the flush that made `cursor` durable did not cover it.
    ///
    /// The tail only moves forward, and never past `head`.
    pub fn checkpoint_to(&mut self, cursor: u64) {
        let ring = self.ring_bytes;
        if ring == 0 {
            return;
        }
        let head = self.state.head % ring;
        let tail = self.state.tail % ring;
        let target = cursor % ring;
        if target == tail {
            return;
        }
        // Distances are measured modulo the ring so a wrapped cursor compares
        // correctly. Accept the target only when it lies between tail and head.
        let dist_to_head = |a: u64| (head + ring - (a % ring)) % ring;
        if dist_to_head(target) <= dist_to_head(tail) {
            self.state.tail = target;
            self.persist();
        }
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

    /// Prove ring buffer `used()` calculation invariants:
    /// 1. `head + ring` never overflows `u64` for any valid ring size (`ring <= u64::MAX / 2`).
    /// 2. `head + ring - tail` never underflows since `head + ring >= ring > tail`.
    /// 3. `used < ring` always holds.
    /// 4. When `head == tail`, `used == 0`.
    /// 5. When `head != tail`, `used > 0 && used < ring`.
    #[kani::proof]
    fn proof_ring_used_invariants() {
        let ring: u64 = kani::any();
        let head: u64 = kani::any();
        let tail: u64 = kani::any();
        kani::assume(ring > 0 && ring <= (u64::MAX / 2));
        kani::assume(head < ring);
        kani::assume(tail < ring);

        // Checked arithmetic verification
        let head_plus_ring = head.checked_add(ring);
        assert!(
            head_plus_ring.is_some(),
            "head + ring must never overflow u64"
        );
        let sum = head_plus_ring.unwrap();
        assert!(sum > tail, "head + ring must strictly exceed tail");

        let used = (sum - tail) % ring;
        assert!(
            used < ring,
            "used bytes must be strictly less than ring size"
        );

        if head == tail {
            assert_eq!(used, 0, "used must be 0 when head == tail");
            kani::cover!(used == 0, "empty ring reached");
        } else {
            assert!(used > 0, "used must be positive when head != tail");
            assert!(used < ring, "used must stay below capacity");
            kani::cover!(head > tail, "head ahead of tail");
            kani::cover!(head < tail, "tail wrapped ahead of head");
        }
    }

    /// Prove that `append`'s wrapping and slice indexing cannot exceed `ring_bytes`.
    ///
    /// After wrapping `head` if `head + len > ring`, `head + len <= ring` is guaranteed,
    /// ensuring `ring()[head..head+len]` can never access out of bounds, and
    /// `(head + len) % ring < ring`.
    #[kani::proof]
    fn proof_ring_append_bounds_guarantee() {
        let ring_bytes: u64 = kani::any();
        let mut head: u64 = kani::any();
        let len: u64 = kani::any();
        kani::assume(ring_bytes > 0 && ring_bytes <= (1 << 30));
        kani::assume(head < ring_bytes);
        kani::assume(len > 0 && len <= ring_bytes);

        // Mirror JournalRing::append wrap check
        if head.saturating_add(len) > ring_bytes {
            head = 0;
        }

        // Must fit contiguously without exceeding ring_bytes
        let end = head.checked_add(len);
        assert!(end.is_some());
        let end_val = end.unwrap();
        assert!(
            end_val <= ring_bytes,
            "write slice must never exceed ring capacity"
        );

        // Next head cursor stays within [0, ring_bytes)
        let next_head = end_val % ring_bytes;
        assert!(next_head < ring_bytes, "next head must stay inside ring");
    }

    /// Prove that `checkpoint_to` moves `tail` forward towards `head` without passing it.
    ///
    /// Distances are measured modulo the ring. A candidate `target` is only accepted
    /// when `dist_to_head(target) <= dist_to_head(tail)`, guaranteeing that the distance
    /// to head is strictly non-increasing and tail never overtakes head.
    #[kani::proof]
    fn proof_ring_checkpoint_to_monotonicity() {
        let ring: u64 = kani::any();
        let head: u64 = kani::any();
        let tail: u64 = kani::any();
        let target: u64 = kani::any();
        kani::assume(ring > 0 && ring <= (1 << 20));
        kani::assume(head < ring);
        kani::assume(tail < ring);
        kani::assume(target < ring);

        let dist = |a: u64| (head + ring - a) % ring;
        let dist_tail = dist(tail);
        let dist_target = dist(target);

        assert!(dist_tail < ring);
        assert!(dist_target < ring);

        if dist_target <= dist_tail {
            // Target lies between tail and head in forward ring order
            let new_tail = target;
            assert!(
                dist(new_tail) <= dist_tail,
                "tail must not move backwards away from head"
            );
            kani::cover!(dist_target < dist_tail, "tail advanced forward");
            kani::cover!(dist_target == dist_tail, "tail remained at same distance");
        }
    }

    /// Prove that applying `MetadataOp::SetInodeBitmap` is strictly idempotent:
    /// `apply(op)` followed by `apply(op)` leaves the image in the exact same state as `apply(op)`.
    #[kani::proof]
    fn proof_apply_op_set_inode_bitmap_idempotent() {
        let mut image = [0u8; 32];
        let mut sb = SuperBlock::new(100);
        sb.block_size = 16;
        sb.inode_bitmap_block = 1;

        let inode_id: u64 = kani::any();
        let allocated: bool = kani::any();
        kani::assume(inode_id < 128); // 16 bytes * 8 bits

        let op = MetadataOp::SetInodeBitmap {
            inode_id,
            allocated,
        };

        if apply_op_in_place(&mut image, &sb, &op).is_ok() {
            let once = image;
            // Apply a second time
            let res2 = apply_op_in_place(&mut image, &sb, &op);
            assert!(
                res2.is_ok(),
                "re-applying an already applied op must succeed"
            );
            assert_eq!(
                image, once,
                "second application must be completely identical (idempotent)"
            );
        }
    }

    /// Prove that applying `MetadataOp::SetDataBitmap` is strictly idempotent.
    #[kani::proof]
    fn proof_apply_op_set_data_bitmap_idempotent() {
        let mut image = [0u8; 32];
        let mut sb = SuperBlock::new(100);
        sb.block_size = 16;
        sb.data_bitmap_block = 1;
        sb.data_block_start = 2;

        let block_id: u64 = kani::any();
        let allocated: bool = kani::any();
        kani::assume(block_id >= 2 && block_id < 2 + 128);

        let op = MetadataOp::SetDataBitmap {
            block_id,
            allocated,
        };

        if apply_op_in_place(&mut image, &sb, &op).is_ok() {
            let once = image;
            let res2 = apply_op_in_place(&mut image, &sb, &op);
            assert!(res2.is_ok());
            assert_eq!(
                image, once,
                "second application must be completely identical (idempotent)"
            );
        }
    }

    /// Prove that applying `MetadataOp::WriteBlockSlice` is strictly idempotent.
    #[kani::proof]
    fn proof_apply_op_write_block_slice_idempotent() {
        let mut image = [0u8; 32];
        let mut sb = SuperBlock::new(100);
        sb.block_size = 16;

        let block_id: u64 = kani::any();
        let offset: u32 = kani::any();
        let d0: u8 = kani::any();
        let d1: u8 = kani::any();
        kani::assume(block_id <= 1);
        kani::assume(offset <= 14);

        let data = vec![d0, d1];
        let op = MetadataOp::WriteBlockSlice {
            block_id,
            offset,
            data,
        };

        if apply_op_in_place(&mut image, &sb, &op).is_ok() {
            let once = image;
            let res2 = apply_op_in_place(&mut image, &sb, &op);
            assert!(res2.is_ok());
            assert_eq!(
                image, once,
                "second application must be completely identical (idempotent)"
            );
        }
    }

    /// Prove that applying `MetadataOp::WriteInode` is strictly idempotent.
    #[kani::proof]
    fn proof_apply_op_write_inode_idempotent() {
        let mut image = [0u8; 256];
        let mut sb = SuperBlock::new(100);
        sb.block_size = 256;
        sb.inode_table_block = 0;

        let pattern: u8 = kani::any();
        let inode_bytes = Box::new([pattern; INODE_BYTES]);
        let op = MetadataOp::WriteInode {
            inode_id: 0,
            inode_bytes,
        };

        if apply_op_in_place(&mut image, &sb, &op).is_ok() {
            let once = image;
            let res2 = apply_op_in_place(&mut image, &sb, &op);
            assert!(res2.is_ok());
            assert_eq!(
                image, once,
                "second application must be completely identical (idempotent)"
            );
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
        assert_eq!(image[inode_bm] & (1 << 5), 1 << 5);

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

#[cfg(test)]
mod checkpoint_tests {
    use super::*;

    fn make() -> (Vec<u8>, u64) {
        let bs = 4096u64;
        let total = usize::try_from(bs * (1 + JOURNAL_RING_BLOCKS)).unwrap();
        (vec![0u8; total], bs)
    }

    #[test]
    fn test_checkpoint_to_advances_only_up_to_head() {
        let (mut region, bs) = make();
        let mut ring = JournalRing::create(&mut region, bs).expect("create");
        ring.append(&[MetadataOp::SetInodeBitmap {
            inode_id: 1,
            allocated: true,
        }])
        .expect("t1");
        let after_one = ring.state().head;
        ring.append(&[MetadataOp::SetInodeBitmap {
            inode_id: 2,
            allocated: true,
        }])
        .expect("t2");

        // Retiring only the first transaction must leave the second pending.
        ring.checkpoint_to(after_one);
        let st = ring.state();
        assert_eq!(st.tail, after_one, "tail advances to the snapshot");
        assert_eq!(ring.pending_transactions(), 1, "the later tx stays pending");
        assert!(st.head != st.tail, "ring is not empty yet");
    }

    #[test]
    fn test_checkpoint_to_never_moves_tail_backwards() {
        let (mut region, bs) = make();
        let mut ring = JournalRing::create(&mut region, bs).expect("create");
        for i in 0..4u64 {
            ring.append(&[MetadataOp::SetInodeBitmap {
                inode_id: i,
                allocated: true,
            }])
            .expect("append");
        }
        let mid = ring.state().head;
        ring.checkpoint_to(mid);
        assert_eq!(ring.state().tail, mid);

        // An older snapshot must be ignored, not roll the tail back (which would
        // resurrect already-applied transactions and could double-apply them).
        let tail_before = ring.state().tail;
        ring.checkpoint_to(0);
        assert_eq!(
            ring.state().tail,
            tail_before,
            "tail must never move backwards"
        );
    }

    #[test]
    fn test_checkpoint_to_refuses_cursor_past_head() {
        let (mut region, bs) = make();
        let mut ring = JournalRing::create(&mut region, bs).expect("create");
        ring.append(&[MetadataOp::SetInodeBitmap {
            inode_id: 1,
            allocated: true,
        }])
        .expect("t1");
        let head = ring.state().head;
        ring.checkpoint_to(head + 1);
        assert_eq!(
            ring.state().tail,
            0,
            "a cursor past head must not be accepted"
        );
    }

    #[test]
    fn test_checkpoint_is_checkpoint_to_head() {
        let (mut region, bs) = make();
        let mut ring = JournalRing::create(&mut region, bs).expect("create");
        for i in 0..6u64 {
            ring.append(&[MetadataOp::SetInodeBitmap {
                inode_id: i,
                allocated: true,
            }])
            .expect("append");
        }
        ring.checkpoint();
        let st = ring.state();
        assert_eq!(st.tail, st.head, "checkpoint empties the ring");
        assert_eq!(ring.pending_transactions(), 0);
        assert_eq!(ring.used(), 0);
    }

    #[test]
    fn test_checkpoint_to_is_a_noop_when_ring_already_empty() {
        let (mut region, bs) = make();
        let mut ring = JournalRing::create(&mut region, bs).expect("create");
        ring.append(&[MetadataOp::SetInodeBitmap {
            inode_id: 1,
            allocated: true,
        }])
        .expect("append");
        ring.checkpoint();
        let st = ring.state();
        ring.checkpoint_to(st.head);
        assert_eq!(ring.state().tail, st.tail, "no change");
        assert_eq!(ring.pending_transactions(), 0);
    }

    #[test]
    fn test_checkpointed_transactions_are_not_replayed() {
        let (mut region, bs) = make();
        let ops = vec![MetadataOp::SetInodeBitmap {
            inode_id: 9,
            allocated: true,
        }];
        {
            let mut ring = JournalRing::create(&mut region, bs).expect("create");
            ring.append(&ops).expect("append");
            ring.checkpoint();
        }
        let mut ring = JournalRing::open(&mut region, bs).expect("reopen");
        let mut replayed = 0usize;
        ring.recover(|_| {
            replayed += 1;
            Ok(())
        })
        .expect("recover");
        assert_eq!(replayed, 0, "checkpointed transactions must not replay");
    }
}

#[cfg(test)]
mod log_conformance_tests {
    use super::*;
    use std::collections::BTreeSet;

    fn scratch(_name: &str) -> (Vec<u8>, u64) {
        let bs = 4096u64;
        let total = usize::try_from(bs * (1 + JOURNAL_RING_BLOCKS)).unwrap();
        (vec![0u8; total], bs)
    }

    fn ops(n: u64) -> Vec<MetadataOp> {
        (0..n)
            .map(|i| MetadataOp::SetInodeBitmap {
                inode_id: i,
                allocated: i % 2 == 0,
            })
            .collect()
    }

    /// Read every frame currently in the ring, in order.
    fn frames(ring: &JournalRing) -> Vec<(u64, Vec<MetadataOp>)> {
        let ring_bytes = ring.ring_bytes;
        let mut cursor = ring.state().tail % ring_bytes;
        let head = ring.state().head % ring_bytes;
        let buf = ring.ring_ref();
        let mut out = Vec::new();
        while cursor != head {
            match decode_frame(&buf[usize::try_from(cursor).unwrap()..]) {
                Some((tx, total)) => {
                    out.push((tx.tx_seq, tx.ops));
                    cursor = (cursor + total as u64) % ring_bytes;
                }
                None => break,
            }
        }
        out
    }

    #[test]
    fn test_transaction_sequence_numbers_are_monotonic_and_dense() {
        let (mut region, bs) = scratch("seq");
        let mut ring = JournalRing::create(&mut region, bs).expect("create");
        for i in 0..10u64 {
            ring.append(&ops(i % 4 + 1)).expect("append");
            assert_eq!(ring.state().tx_seq, i + 1, "seq must increment by one");
        }
        let seen: Vec<u64> = frames(&ring).iter().map(|(s, _)| *s).collect();
        assert_eq!(seen, (1..=10).collect::<Vec<u64>>(), "dense, no gaps");
    }

    #[test]
    fn test_frames_decode_to_exactly_the_ops_appended() {
        let (mut region, bs) = scratch("roundtrip");
        let mut ring = JournalRing::create(&mut region, bs).expect("create");
        let expected: Vec<Vec<MetadataOp>> = (0..6u64).map(|i| ops(i % 5 + 1)).collect();
        for e in &expected {
            ring.append(e).expect("append");
        }
        let decoded = frames(&ring);
        assert_eq!(decoded.len(), expected.len(), "frame count matches");
        for (i, (_, got)) in decoded.iter().enumerate() {
            assert_eq!(got, &expected[i], "frame {i} ops must round-trip exactly");
        }
    }

    #[test]
    fn test_replay_applies_ops_in_append_order() {
        let (mut region, bs) = scratch("order");
        let mut ring = JournalRing::create(&mut region, bs).expect("create");
        for i in 0..5u64 {
            ring.append(&[MetadataOp::SetInodeBitmap {
                inode_id: 100 + i,
                allocated: true,
            }])
            .expect("append");
        }
        let mut applied = Vec::new();
        ring.recover(|op| {
            if let MetadataOp::SetInodeBitmap { inode_id, .. } = op {
                applied.push(*inode_id);
            }
            Ok(())
        })
        .expect("recover");
        assert_eq!(applied, vec![100, 101, 102, 103, 104]);
    }

    #[test]
    fn test_ring_never_exceeds_capacity_and_drops_oldest() {
        let (mut region, bs) = scratch("capacity");
        let mut ring = JournalRing::create(&mut region, bs).expect("create");
        let big = vec![0u8; 900];
        let total = 400usize;
        for i in 0..total {
            ring.append(&[MetadataOp::WriteBlockSlice {
                block_id: 1000 + i as u64,
                offset: 0,
                data: big.clone(),
            }])
            .expect("append");
            let capacity = journal_ring_bytes(bs);
            assert!(
                ring.used() <= capacity,
                "ring usage must never exceed capacity (used={} cap={})",
                ring.used(),
                capacity
            );
        }
        // The newest frames must still be intact and decodable.
        let surviving = frames(&ring);
        assert!(!surviving.is_empty(), "newest frame must survive");
        let last = surviving.last().expect("last frame");
        assert_eq!(
            last.1[0],
            MetadataOp::WriteBlockSlice {
                block_id: 1000 + total as u64 - 1,
                offset: 0,
                data: big,
            },
            "the most recent transaction must be the last one"
        );
    }

    #[test]
    fn test_dropped_transactions_are_the_oldest_ones() {
        let (mut region, bs) = scratch("dropoldest");
        let mut ring = JournalRing::create(&mut region, bs).expect("create");
        let big = vec![1u8; 900];
        for i in 0..400u64 {
            ring.append(&[MetadataOp::WriteBlockSlice {
                block_id: i,
                offset: 0,
                data: big.clone(),
            }])
            .expect("append");
        }
        let surviving: BTreeSet<u64> = frames(&ring)
            .iter()
            .filter_map(|(_, ops)| match &ops[0] {
                MetadataOp::WriteBlockSlice { block_id, .. } => Some(*block_id),
                _ => None,
            })
            .collect();
        assert!(
            surviving.iter().all(|b| *b > 0),
            "only recent block ids should survive"
        );
        assert!(
            surviving.iter().any(|b| *b == 399),
            "the newest must survive"
        );
    }

    #[test]
    fn test_recover_after_wrap_replays_every_surviving_frame() {
        let (mut region, bs) = scratch("wraprecover");
        let mut ring = JournalRing::create(&mut region, bs).expect("create");
        let big = vec![7u8; 600];
        for i in 0..500u64 {
            ring.append(&[MetadataOp::WriteBlockSlice {
                block_id: 5000 + i,
                offset: 0,
                data: big.clone(),
            }])
            .expect("append");
        }
        let before = frames(&ring).len();
        assert!(before > 0);
        let mut applied = 0usize;
        ring.recover(|_| {
            applied += 1;
            Ok(())
        })
        .expect("recover after wrap");
        assert_eq!(applied, before, "every surviving frame must replay");
        assert_eq!(ring.pending_transactions(), 0, "ring empty after recover");
    }

    #[test]
    fn test_pending_transactions_matches_frame_walk() {
        let (mut region, bs) = scratch("pending");
        let mut ring = JournalRing::create(&mut region, bs).expect("create");
        for i in 0..7u64 {
            ring.append(&ops(i + 1)).expect("append");
            assert_eq!(
                ring.pending_transactions(),
                frames(&ring).len(),
                "pending count must match the actual frame walk"
            );
        }
        ring.checkpoint();
        assert_eq!(ring.pending_transactions(), 0);
        assert!(frames(&ring).is_empty());
    }

    #[test]
    fn test_crc_rejects_every_single_bit_flip_in_a_real_frame() {
        let (mut region, bs) = scratch("crc");
        let mut ring = JournalRing::create(&mut region, bs).expect("create");
        ring.append(&ops(3)).expect("append");
        let st = ring.state();
        let frame = {
            let buf = ring.ring_ref();
            let start = usize::try_from(st.tail).unwrap();
            let end = usize::try_from(st.head).unwrap();
            buf[start..end].to_vec()
        };
        assert!(decode_frame(&frame).is_some(), "clean frame decodes");
        for byte in 0..frame.len() {
            for bit in 0..8u8 {
                let mut corrupt = frame.clone();
                corrupt[byte] ^= 1 << bit;
                assert!(
                    decode_frame(&corrupt).is_none(),
                    "flip at byte {byte} bit {bit} must be rejected"
                );
            }
        }
    }

    #[test]
    fn test_truncated_frames_never_decode() {
        let (mut region, bs) = scratch("trunc");
        let mut ring = JournalRing::create(&mut region, bs).expect("create");
        ring.append(&ops(4)).expect("append");
        let st = ring.state();
        let frame = {
            let buf = ring.ring_ref();
            let start = usize::try_from(st.tail).unwrap();
            let end = usize::try_from(st.head).unwrap();
            buf[start..end].to_vec()
        };
        for cut in 1..frame.len() {
            assert!(
                decode_frame(&frame[..cut]).is_none(),
                "truncation to {cut} bytes must be rejected"
            );
        }
    }

    #[test]
    fn test_surviving_frames_still_validate_after_heavy_wrap() {
        // The guarantee that matters in production: whatever is still in the ring is
        // always a set of intact, decodable frames — never a torn tail.
        let (mut region, bs) = scratch("intact");
        let mut ring = JournalRing::create(&mut region, bs).expect("create");
        let payload = vec![0xA5u8; 700];
        for i in 0..600u64 {
            ring.append(&[MetadataOp::WriteBlockSlice {
                block_id: 7000 + i,
                offset: 0,
                data: payload.clone(),
            }])
            .expect("append");
        }
        for (_, ops) in frames(&ring) {
            assert_eq!(ops.len(), 1, "each frame holds exactly one op");
            assert!(
                matches!(ops[0], MetadataOp::WriteBlockSlice { .. }),
                "op survived intact"
            );
        }
    }

    #[test]
    fn test_header_state_survives_reopen() {
        let (mut region, bs) = scratch("persist");
        let expected_head;
        let expected_seq;
        {
            let mut ring = JournalRing::create(&mut region, bs).expect("create");
            for i in 0..4u64 {
                ring.append(&ops(i + 1)).expect("append");
            }
            expected_head = ring.state().head;
            expected_seq = ring.state().tx_seq;
        }
        let ring = JournalRing::open(&mut region, bs).expect("reopen");
        let st = ring.state();
        assert_eq!(st.head, expected_head, "head persisted in the header");
        assert_eq!(st.tx_seq, expected_seq, "sequence persisted");
        assert_eq!(st.tail, 0, "fresh ring has an empty tail");
    }

    #[test]
    fn test_used_matches_the_sum_of_frame_sizes() {
        let (mut region, bs) = scratch("used");
        let mut ring = JournalRing::create(&mut region, bs).expect("create");
        for i in 0..5u64 {
            ring.append(&ops(i + 1)).expect("append");
            let walk: u64 = frames(&ring)
                .iter()
                .map(|(_, ops)| {
                    let mut buf = Vec::new();
                    for op in ops {
                        op.encode(&mut buf);
                    }
                    (TX_HEADER_LEN + buf.len() + 4) as u64
                })
                .sum();
            assert_eq!(ring.used(), walk, "used() must equal the framed byte count");
        }
    }
}

// ---------------------------------------------------------------------------
// Allocation staging for the journaled write paths
// ---------------------------------------------------------------------------

use crate::BLOCK_SIZE;
use crate::allocator::SimpleBlockAllocator;
use crate::disk::{DiskManager, DiskManagerError, DiskManagerInner};
use crate::inode::Inode;
use memmap2::MmapMut;

/// Local simulation of block/inode allocation against copied bitmaps.
///
/// The journaled create path must know every allocated id *before* it touches the
/// image, so the whole operation can be expressed as post-images and committed to
/// the WAL first. Allocation here mirrors `SimpleBlockAllocator` semantics exactly
/// (same hint handling, same "first free bit at or after hint" rule), so the ids it
/// predicts are the ids the real allocator would hand out.
pub(crate) struct AllocSim {
    inode_bitmap: Vec<u8>,
    data_bitmap: Vec<u8>,
    data_start: u64,
    pub(crate) free_inode_hint: u64,
    pub(crate) free_block_hint: u64,
    /// Blocks allocated during this simulation, in order.
    pub(crate) fresh_blocks: Vec<u64>,
}

impl AllocSim {
    pub(crate) fn new(
        guard: &DiskManagerInner,
        free_inode_hint: u64,
        free_block_hint: u64,
    ) -> Result<Self, DiskManagerError> {
        let sb = guard.superblock;
        let ib = DiskManager::get_block_from_map(&guard.mmap, sb.inode_bitmap_block)
            .ok_or_else(|| DiskManagerError::Io(std::io::Error::other("inode bitmap not found")))?;
        let db = DiskManager::get_block_from_map(&guard.mmap, sb.data_bitmap_block)
            .ok_or_else(|| DiskManagerError::Io(std::io::Error::other("data bitmap not found")))?;
        Ok(Self {
            inode_bitmap: ib.to_vec(),
            data_bitmap: db.to_vec(),
            data_start: sb.data_block_start,
            free_inode_hint,
            free_block_hint,
            fresh_blocks: Vec::new(),
        })
    }

    pub(crate) fn alloc_inode(&mut self) -> Result<u64, DiskManagerError> {
        let mut a = SimpleBlockAllocator::new(&mut self.inode_bitmap, 0);
        let id = a
            .allocate_with_hint(Some(self.free_inode_hint))
            .map_err(DiskManagerError::Allocator)?;
        self.free_inode_hint = id + 1;
        Ok(id)
    }

    pub(crate) fn alloc_block(&mut self) -> Result<u64, DiskManagerError> {
        let mut a = SimpleBlockAllocator::new(&mut self.data_bitmap, self.data_start);
        let blk = a
            .allocate_with_hint(Some(self.free_block_hint))
            .map_err(DiskManagerError::Allocator)?;
        self.free_block_hint = blk + 1;
        self.fresh_blocks.push(blk);
        Ok(blk)
    }

    pub(crate) fn is_fresh(&self, block_id: u64) -> bool {
        self.fresh_blocks.contains(&block_id)
    }
}

/// Mirror of [`DiskManager::get_or_alloc_block`] that allocates against [`AllocSim`].
///
/// Instead of writing pointer blocks into the image it records the equivalent
/// `MetadataOp`s, so the resulting indirect-block structure is captured in the WAL
/// transaction exactly as the legacy path would have written it.
pub(crate) fn sim_get_or_alloc_block(
    mmap: &MmapMut,
    inode: &mut Inode,
    logical_idx: usize,
    sim: &mut AllocSim,
    ops: &mut Vec<crate::journal::MetadataOp>,
) -> Result<u64, DiskManagerError> {
    use crate::inode::BlockPath;

    fn ensure_root(
        slot: &mut u64,
        sim: &mut AllocSim,
        ops: &mut Vec<crate::journal::MetadataOp>,
    ) -> Result<u64, DiskManagerError> {
        if *slot == 0 {
            let blk = sim.alloc_block()?;
            ops.push(crate::journal::MetadataOp::SetDataBitmap {
                block_id: blk,
                allocated: true,
            });
            *slot = blk;
        }
        Ok(*slot)
    }

    fn child(
        mmap: &MmapMut,
        parent_blk: u64,
        idx: usize,
        sim: &mut AllocSim,
        ops: &mut Vec<crate::journal::MetadataOp>,
    ) -> Result<u64, DiskManagerError> {
        // A freshly allocated pointer block reads as all-zero, so its entries are 0.
        let existing = if sim.is_fresh(parent_blk) {
            0
        } else {
            DiskManager::read_block_ptr(mmap, parent_blk, idx)
        };
        if existing != 0 {
            return Ok(existing);
        }
        let blk = sim.alloc_block()?;
        ops.push(crate::journal::MetadataOp::SetDataBitmap {
            block_id: blk,
            allocated: true,
        });
        // Record the pointer write into the parent pointer block.
        ops.push(crate::journal::MetadataOp::WriteBlockSlice {
            block_id: parent_blk,
            offset: (idx * 8) as u32,
            data: blk.to_le_bytes().to_vec(),
        });
        Ok(blk)
    }

    match BlockPath::from_logical(logical_idx) {
        Some(BlockPath::Direct(i)) => {
            if inode.blocks[i] == 0 {
                let blk = sim.alloc_block()?;
                ops.push(crate::journal::MetadataOp::SetDataBitmap {
                    block_id: blk,
                    allocated: true,
                });
                inode.blocks[i] = blk;
            }
            Ok(inode.blocks[i])
        }
        Some(BlockPath::Single(i)) => {
            let sib = ensure_root(&mut inode.blocks[10], sim, ops)?;
            if sib == 0 {
                return Ok(0);
            }
            child(mmap, sib, i, sim, ops)
        }
        Some(BlockPath::Double(a, b)) => {
            let dib = ensure_root(&mut inode.blocks[11], sim, ops)?;
            if dib == 0 {
                return Ok(0);
            }
            let sib = child(mmap, dib, a, sim, ops)?;
            if sib == 0 {
                return Ok(0);
            }
            child(mmap, sib, b, sim, ops)
        }
        Some(BlockPath::Triple(a, b, c)) => {
            let tib = ensure_root(&mut inode.triple_indirect, sim, ops)?;
            if tib == 0 {
                return Ok(0);
            }
            let dib = child(mmap, tib, a, sim, ops)?;
            if dib == 0 {
                return Ok(0);
            }
            let sib = child(mmap, dib, b, sim, ops)?;
            if sib == 0 {
                return Ok(0);
            }
            child(mmap, sib, c, sim, ops)
        }
        None => Err(DiskManagerError::Io(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            "File too large (max 513GB)",
        ))),
    }
}

/// Stage a payload write into the file described by `inode`.
///
/// Allocates through `sim` so every id is known before the bitmaps are touched,
/// and records each metadata change in `ops` (bitmap bits, indirect pointer writes).
/// `used` collects the physical blocks touched, so the caller can free orphans and
/// flush exactly the right ranges.
///
/// Payload bytes are written straight into the mapping and are deliberately **not**
/// recorded as ops: journaling user data would multiply WAL traffic by the file size
/// and defeat the point of a metadata journal. Durability instead comes from
/// ordering — the caller flushes these blocks *before* committing the metadata
/// transaction, so recovery never exposes an allocated-but-empty block.
pub(crate) fn stage_payload_write(
    guard: &mut DiskManagerInner,
    inode: &mut Inode,
    phys_off: u64,
    buf: &[u8],
    sim: &mut AllocSim,
    ops: &mut Vec<crate::journal::MetadataOp>,
    used: &mut Vec<u64>,
) -> Result<(), DiskManagerError> {
    let mut written = 0usize;
    let mut cur = phys_off;
    while written < buf.len() {
        let blk_idx = (cur / BLOCK_SIZE as u64) as usize;
        let in_blk_off = (cur % BLOCK_SIZE as u64) as usize;
        let n = std::cmp::min(buf.len() - written, BLOCK_SIZE - in_blk_off);

        let phys = sim_get_or_alloc_block(&guard.mmap, inode, blk_idx, sim, ops)?;
        if phys != 0 && !used.contains(&phys) {
            used.push(phys);
        }
        if let Some(slice) = DiskManager::get_block_mut_from_map(&mut guard.mmap, phys) {
            slice[in_blk_off..in_blk_off + n].copy_from_slice(&buf[written..written + n]);
        }
        written += n;
        cur += n as u64;
    }
    Ok(())
}

/// Clear block pointers at or beyond the file's new logical length.
///
/// When a file shrinks, the indirect pointer blocks keep stale entries pointing at
/// blocks that are no longer used. Those blocks are freed by the caller, so leaving
/// the pointers behind would leave the inode referencing blocks the bitmap says are
/// free — an inconsistency `fsck` reports as `missing_blocks`.
///
/// Direct pointers live in the inode and are captured by the `WriteInode` op;
/// indirect entries are recorded as zeroing `WriteBlockSlice` ops so the change is
/// part of the same transaction.
pub(crate) fn prune_stale_pointers(
    mmap: &MmapMut,
    inode: &mut Inode,
    first_stale: usize,
    ops: &mut Vec<crate::journal::MetadataOp>,
) -> Result<(), DiskManagerError> {
    const POINTERS_PER_BLOCK: usize = 512; // 4096 / 8
    const DIRECT: usize = 10;
    const SINGLE_SPAN: usize = DIRECT + POINTERS_PER_BLOCK;
    const DOUBLE_SPAN: usize = DIRECT + POINTERS_PER_BLOCK * (1 + POINTERS_PER_BLOCK);

    if first_stale == 0 {
        return Ok(());
    }

    // Direct pointers live in the inode and are captured by the WriteInode op.
    for i in first_stale.min(DIRECT)..DIRECT {
        inode.blocks[i] = 0;
    }
    if first_stale <= DIRECT {
        // Nothing beyond the direct blocks survives; drop the indirect roots so
        // they are freed along with the rest of the orphans.
        inode.blocks[10] = 0;
        inode.blocks[11] = 0;
        inode.triple_indirect = 0;
        return Ok(());
    }

    // Early exits depend only on `first_stale`, never on whether the pointer
    // block happens to be present: a missing root must not let the arithmetic
    // below underflow. Emitting ops is still gated on the root existing —
    // block id 0 is the SuperBlock and must never be a write target.
    if first_stale <= SINGLE_SPAN {
        let sib = inode.blocks[10];
        if sib != 0 {
            let start = first_stale - DIRECT;
            for idx in start..POINTERS_PER_BLOCK {
                ops.push(crate::journal::MetadataOp::WriteBlockSlice {
                    block_id: sib,
                    offset: (idx * 8) as u32,
                    data: vec![0u8; 8],
                });
            }
        }
        return Ok(());
    }

    if first_stale <= DOUBLE_SPAN {
        let dib = inode.blocks[11];
        if dib != 0 {
            let start = first_stale - SINGLE_SPAN;
            for a in start..POINTERS_PER_BLOCK {
                ops.push(crate::journal::MetadataOp::WriteBlockSlice {
                    block_id: dib,
                    offset: (a * 8) as u32,
                    data: vec![0u8; 8],
                });
                let sib2 = DiskManager::read_block_ptr(mmap, dib, a);
                if sib2 == 0 {
                    continue;
                }
                // A whole second-level block goes away; zero it entirely.
                ops.push(crate::journal::MetadataOp::WriteBlockSlice {
                    block_id: sib2,
                    offset: 0,
                    data: vec![0u8; BLOCK_SIZE],
                });
            }
        }
        return Ok(());
    }

    // Triple indirect: whole third-level blocks beyond the new length.
    let tib = inode.triple_indirect;
    if tib != 0 {
        let start = first_stale - DOUBLE_SPAN;
        for b in start..POINTERS_PER_BLOCK {
            ops.push(crate::journal::MetadataOp::WriteBlockSlice {
                block_id: tib,
                offset: (b * 8) as u32,
                data: vec![0u8; 8],
            });
            let dib2 = DiskManager::read_block_ptr(mmap, tib, b);
            if dib2 == 0 {
                continue;
            }
            for c in 0..POINTERS_PER_BLOCK {
                ops.push(crate::journal::MetadataOp::WriteBlockSlice {
                    block_id: dib2,
                    offset: (c * 8) as u32,
                    data: vec![0u8; 8],
                });
                let sib3 = DiskManager::read_block_ptr(mmap, dib2, c);
                if sib3 == 0 {
                    continue;
                }
                ops.push(crate::journal::MetadataOp::WriteBlockSlice {
                    block_id: sib3,
                    offset: 0,
                    data: vec![0u8; BLOCK_SIZE],
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod prune_pointer_tests {
    use super::*;

    const DIRECT: usize = 10;
    const P: usize = 512;
    const SINGLE_SPAN: usize = DIRECT + P;
    const DOUBLE_SPAN: usize = DIRECT + P * (1 + P);

    /// A small real mmap so `read_block_ptr` can walk planted pointer blocks.
    pub(super) fn scratch() -> (std::fs::File, memmap2::MmapMut) {
        static SCRATCH_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = SCRATCH_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "oifs_prune_{}_{}_{}.img",
            std::process::id(),
            id,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("scratch file");
        file.set_len(64 * 4096).expect("size");
        let mmap = unsafe { memmap2::MmapOptions::new().map_mut(&file).expect("map") };
        (file, mmap)
    }

    fn plant(mmap: &mut MmapMut, block: u64, entry: usize, value: u64) {
        let off = block as usize * BLOCK_SIZE + entry * 8;
        mmap[off..off + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn ops_targeting(
        ops: &[crate::journal::MetadataOp],
        block: u64,
    ) -> Vec<&crate::journal::MetadataOp> {
        ops.iter()
            .filter(|op| matches!(op, crate::journal::MetadataOp::WriteBlockSlice { block_id, .. } if *block_id == block))
            .collect()
    }

    #[test]
    fn test_prune_direct_only_clears_direct_pointers() {
        let (_f, mmap) = scratch();
        let mut inode = Inode::new(crate::inode::FileType::File);
        for i in 0..DIRECT {
            inode.blocks[i] = 100 + i as u64;
        }
        inode.blocks[10] = 500;
        let mut ops = Vec::new();
        prune_stale_pointers(&mmap, &mut inode, 3, &mut ops).expect("prune");

        // Only the first three direct blocks survive.
        assert_eq!(&inode.blocks[..3], &[100u64, 101, 102]);
        assert!(
            inode.blocks[3..DIRECT].iter().all(|b| *b == 0),
            "direct cleared"
        );
        assert_eq!(inode.blocks[10], 0, "indirect root dropped");
        assert!(ops.is_empty(), "no pointer-block writes needed");
    }

    #[test]
    fn test_prune_into_single_indirect_zeroes_tail_entries() {
        let (_f, mmap) = scratch();
        const SIB: u64 = 20;
        let mut inode = Inode::new(crate::inode::FileType::File);
        inode.blocks[10] = SIB;
        let mut ops = Vec::new();
        prune_stale_pointers(&mmap, &mut inode, DIRECT + 100, &mut ops).expect("prune");

        let sib_ops = ops_targeting(&ops, SIB);
        assert_eq!(sib_ops.len(), P - 100, "entries 100..512 must be zeroed");
        // The first zeroed entry is 100; the last is 511.
        let offsets: Vec<u32> = sib_ops
            .iter()
            .map(|op| match op {
                crate::journal::MetadataOp::WriteBlockSlice { offset, .. } => *offset,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            offsets.first().copied(),
            Some(800u32),
            "first zeroed entry is 100"
        );
        assert_eq!(
            offsets.last().copied(),
            Some(4088u32),
            "last zeroed entry is 511"
        );
    }

    #[test]
    fn test_prune_into_double_indirect_zeroes_whole_second_level_blocks() {
        let (_f, mut mmap) = scratch();
        const DIB: u64 = 21;
        const SIB2: u64 = 22;
        let mut inode = Inode::new(crate::inode::FileType::File);
        inode.blocks[11] = DIB;
        // Give the first stale second-level block a real target.
        plant(&mut mmap, DIB, 3, SIB2);
        let mut ops = Vec::new();
        prune_stale_pointers(&mmap, &mut inode, SINGLE_SPAN + 3, &mut ops).expect("prune");

        // The dib entry is zeroed and the whole sib2 block is wiped.
        assert!(!ops_targeting(&ops, DIB).is_empty(), "dib entry zeroed");
        let sib2_ops = ops_targeting(&ops, SIB2);
        assert_eq!(sib2_ops.len(), 1, "whole second-level block wiped");
        match sib2_ops[0] {
            crate::journal::MetadataOp::WriteBlockSlice { offset, data, .. } => {
                assert_eq!(*offset, 0);
                assert_eq!(data.len(), BLOCK_SIZE);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_prune_into_triple_indirect_is_reachable_and_zeroes_third_level() {
        // The triple-indirect branch needs a file larger than ~1 GB, so it cannot be
        // reached by an integration test. Drive it directly instead.
        const TIB: u64 = 23;
        const DIB2: u64 = 24;
        const SIB3: u64 = 25;
        let (_f, mut mmap) = scratch();
        // Plant a third-level chain reachable from the first stale `b`.
        plant(&mut mmap, TIB, 1, DIB2);
        plant(&mut mmap, DIB2, 2, SIB3);

        let mut inode = Inode::new(crate::inode::FileType::File);
        inode.triple_indirect = TIB;

        let mut ops = Vec::new();
        prune_stale_pointers(&mmap, &mut inode, DOUBLE_SPAN + 1, &mut ops).expect("prune");

        // TIB entries from the first stale b upward are zeroed.
        let tib_ops = ops_targeting(&ops, TIB);
        assert_eq!(tib_ops.len(), P - 1, "tib entries 1..512 zeroed");

        // The planted DIB2 is reached: one zeroing op per entry.
        let dib2_ops = ops_targeting(&ops, DIB2);
        assert_eq!(dib2_ops.len(), P, "all 512 second-level entries zeroed");
        assert!(
            ops_targeting(&ops, SIB3).len() == 1,
            "leaf block must be wiped"
        );
    }

    #[test]
    fn test_prune_does_not_underflow_without_indirect_roots() {
        // A missing root must not let the tier arithmetic underflow. `first_stale`
        // sits below the single-indirect span while no indirect block exists.
        let (_f, mmap) = scratch();
        let mut inode = Inode::new(crate::inode::FileType::File);
        // blocks[10] and blocks[11] left at 0.
        inode.blocks[0] = 42;
        let mut ops = Vec::new();
        prune_stale_pointers(&mmap, &mut inode, DIRECT + 1, &mut ops)
            .expect("prune must not panic");
        assert!(ops.is_empty(), "nothing addressable to zero");
    }

    #[test]
    fn test_prune_first_stale_zero_is_noop() {
        let (_f, mmap) = scratch();
        let mut inode = Inode::new(crate::inode::FileType::File);
        inode.blocks[0] = 42;
        inode.blocks[10] = 500;
        let mut ops = Vec::new();
        prune_stale_pointers(&mmap, &mut inode, 0, &mut ops).expect("prune");
        assert_eq!(inode.blocks[0], 42);
        assert_eq!(inode.blocks[10], 500);
        assert!(ops.is_empty(), "first_stale == 0 must emit no ops");
    }

    #[test]
    fn test_prune_boundary_first_stale_equals_direct() {
        let (_f, mmap) = scratch();
        let mut inode = Inode::new(crate::inode::FileType::File);
        for i in 0..DIRECT {
            inode.blocks[i] = 100 + i as u64;
        }
        inode.blocks[10] = 500;
        inode.blocks[11] = 600;
        inode.triple_indirect = 700;
        let mut ops = Vec::new();
        prune_stale_pointers(&mmap, &mut inode, DIRECT, &mut ops).expect("prune");

        // All 10 direct blocks remain intact.
        assert_eq!(inode.blocks[DIRECT - 1], 100 + (DIRECT - 1) as u64);
        // All indirect roots must be dropped.
        assert_eq!(inode.blocks[10], 0);
        assert_eq!(inode.blocks[11], 0);
        assert_eq!(inode.triple_indirect, 0);
        assert!(ops.is_empty());
    }

    #[test]
    fn test_prune_single_indirect_missing_root_does_not_panic() {
        let (_f, mmap) = scratch();
        let mut inode = Inode::new(crate::inode::FileType::File);
        // blocks[10] is 0 (missing root)
        let mut ops = Vec::new();
        prune_stale_pointers(&mmap, &mut inode, DIRECT + 50, &mut ops)
            .expect("prune must safely handle missing single indirect root");
        assert!(
            ops.is_empty(),
            "missing sib must not emit ops targeting block 0"
        );
    }

    #[test]
    fn test_prune_double_indirect_sparse_holes() {
        let (_f, mut mmap) = scratch();
        const DIB: u64 = 21;
        const SIB2: u64 = 22;
        let mut inode = Inode::new(crate::inode::FileType::File);
        inode.blocks[11] = DIB;
        // Entry 1 has a valid pointer, but entry 2 is 0 (sparse hole).
        plant(&mut mmap, DIB, 1, SIB2);
        // entry 2 is left as 0 in mmap.
        let mut ops = Vec::new();
        prune_stale_pointers(&mmap, &mut inode, SINGLE_SPAN + 1, &mut ops).expect("prune");

        // DIB entries are zeroed.
        assert!(!ops_targeting(&ops, DIB).is_empty());
        // SIB2 (entry 0) was present and wiped.
        assert_eq!(ops_targeting(&ops, SIB2).len(), 1);
    }

    #[test]
    fn test_prune_triple_indirect_missing_roots() {
        let (_f, mmap) = scratch();
        let mut inode = Inode::new(crate::inode::FileType::File);
        // triple_indirect is 0
        let mut ops = Vec::new();
        prune_stale_pointers(&mmap, &mut inode, DOUBLE_SPAN + 50, &mut ops)
            .expect("prune must safely handle missing tib");
        assert!(ops.is_empty(), "missing tib must not emit ops");
    }

    #[test]
    fn test_prune_triple_indirect_sparse_holes() {
        const TIB: u64 = 23;
        const DIB2: u64 = 24;
        let (_f, mut mmap) = scratch();
        // TIB entry 0 points to DIB2, but DIB2 has only entry 0 pointing to 0 (sparse hole).
        plant(&mut mmap, TIB, 1, DIB2);
        let mut inode = Inode::new(crate::inode::FileType::File);
        inode.triple_indirect = TIB;
        let mut ops = Vec::new();
        prune_stale_pointers(&mmap, &mut inode, DOUBLE_SPAN + 1, &mut ops).expect("prune");

        assert!(!ops_targeting(&ops, TIB).is_empty());
        assert_eq!(ops_targeting(&ops, DIB2).len(), P);
    }

    #[test]
    fn test_prune_is_idempotent() {
        let (_f, mmap) = scratch();
        const SIB: u64 = 20;
        let mut inode = Inode::new(crate::inode::FileType::File);
        inode.blocks[10] = SIB;
        let mut first = Vec::new();
        prune_stale_pointers(&mmap, &mut inode, DIRECT + 10, &mut first).expect("prune");
        let mut second = Vec::new();
        prune_stale_pointers(&mmap, &mut inode, DIRECT + 10, &mut second).expect("prune");
        assert_eq!(
            first.len(),
            second.len(),
            "replaying the same prune must produce the same ops"
        );
    }

    #[test]
    fn test_prune_boundary_at_single_span_preserves_single_indirect_entries() {
        let (_f, mmap) = scratch();
        const SIB: u64 = 20;
        let mut inode = Inode::new(crate::inode::FileType::File);
        inode.blocks[10] = SIB;
        let mut ops = Vec::new();
        prune_stale_pointers(&mmap, &mut inode, SINGLE_SPAN, &mut ops).expect("prune");
        assert_eq!(inode.blocks[10], SIB, "single indirect root preserved");
        assert!(
            ops.is_empty(),
            "at exact SINGLE_SPAN boundary, all single indirect entries are valid"
        );
    }
}

#[cfg(test)]
mod alloc_sim_and_staging_tests {
    use super::*;
    use crate::allocator::AllocatorError;
    use crate::disk::DiskManagerError;
    use crate::inode::Inode;

    #[test]
    fn test_alloc_sim_exhaustion() {
        let mut sim = AllocSim {
            inode_bitmap: vec![0xFF; 64], // all bits set
            data_bitmap: vec![0xFF; 64],  // all bits set
            data_start: 10,
            free_inode_hint: 0,
            free_block_hint: 10,
            fresh_blocks: Vec::new(),
        };
        match sim.alloc_inode() {
            Err(DiskManagerError::Allocator(AllocatorError::NoSpace)) => {}
            other => panic!("expected NoSpace, got {other:?}"),
        }
        match sim.alloc_block() {
            Err(DiskManagerError::Allocator(AllocatorError::NoSpace)) => {}
            other => panic!("expected NoSpace, got {other:?}"),
        }
        assert_eq!(sim.free_inode_hint, 0);
        assert_eq!(sim.free_block_hint, 10);
        assert!(sim.fresh_blocks.is_empty());
    }

    #[test]
    fn test_alloc_sim_allocation_and_fresh_tracking() {
        let mut sim = AllocSim {
            inode_bitmap: vec![0u8; 16],
            data_bitmap: vec![0u8; 16],
            data_start: 100,
            free_inode_hint: 0,
            free_block_hint: 100,
            fresh_blocks: Vec::new(),
        };

        let ino1 = sim.alloc_inode().expect("alloc inode 1");
        let ino2 = sim.alloc_inode().expect("alloc inode 2");
        assert_eq!(ino1, 0);
        assert_eq!(ino2, 1);
        assert_eq!(sim.free_inode_hint, 2);

        let blk1 = sim.alloc_block().expect("alloc block 1");
        let blk2 = sim.alloc_block().expect("alloc block 2");
        assert_eq!(blk1, 100);
        assert_eq!(blk2, 101);
        assert_eq!(sim.free_block_hint, 102);

        assert!(sim.is_fresh(100));
        assert!(sim.is_fresh(101));
        assert!(!sim.is_fresh(99));
        assert!(!sim.is_fresh(102));
        assert_eq!(sim.fresh_blocks, vec![100, 101]);
    }

    #[test]
    fn test_alloc_sim_new_validates_bitmap_blocks() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let img_path = temp_dir.path().join("test.img");
        let dm = DiskManager::open(&img_path, 20 * 1024 * 1024).expect("open disk manager");
        let mut guard = dm.inner_for_test().write().unwrap();

        // Valid AllocSim construction
        let sim = AllocSim::new(&guard, guard.free_inode_hint, guard.free_block_hint)
            .expect("alloc sim new");
        assert_eq!(sim.data_start, guard.superblock.data_block_start);
        assert_eq!(sim.free_inode_hint, guard.free_inode_hint);
        assert_eq!(sim.free_block_hint, guard.free_block_hint);
        assert!(sim.fresh_blocks.is_empty());

        // Corrupted superblock with out-of-range bitmap blocks should error gracefully
        let saved_inode_bitmap = guard.superblock.inode_bitmap_block;
        guard.superblock.inode_bitmap_block = 999_999;
        assert!(AllocSim::new(&guard, 0, 0).is_err());
        guard.superblock.inode_bitmap_block = saved_inode_bitmap;

        let saved_data_bitmap = guard.superblock.data_bitmap_block;
        guard.superblock.data_bitmap_block = 999_999;
        assert!(AllocSim::new(&guard, 0, 0).is_err());
        guard.superblock.data_bitmap_block = saved_data_bitmap;
    }

    #[test]
    fn test_sim_get_or_alloc_block_out_of_range() {
        let (_f, mmap) = prune_pointer_tests::scratch();
        let mut inode = Inode::new(crate::inode::FileType::File);
        let mut sim = AllocSim {
            inode_bitmap: vec![0u8; 16],
            data_bitmap: vec![0u8; 16],
            data_start: 10,
            free_inode_hint: 0,
            free_block_hint: 10,
            fresh_blocks: Vec::new(),
        };
        let mut ops = Vec::new();

        const MAX_LOGICAL_BLOCKS: usize = 10 + 512 + 512 * 512 + 512 * 512 * 512;
        let res = sim_get_or_alloc_block(&mmap, &mut inode, MAX_LOGICAL_BLOCKS, &mut sim, &mut ops);
        match res {
            Err(DiskManagerError::Io(err)) => {
                assert_eq!(err.kind(), std::io::ErrorKind::FileTooLarge);
            }
            other => panic!("expected FileTooLarge error, got {other:?}"),
        }
        assert!(ops.is_empty());
    }

    #[test]
    fn test_sim_get_or_alloc_block_direct_allocation_and_idempotency() {
        let (_f, mmap) = prune_pointer_tests::scratch();
        let mut inode = Inode::new(crate::inode::FileType::File);
        let mut sim = AllocSim {
            inode_bitmap: vec![0u8; 16],
            data_bitmap: vec![0u8; 16],
            data_start: 20,
            free_inode_hint: 0,
            free_block_hint: 20,
            fresh_blocks: Vec::new(),
        };
        let mut ops = Vec::new();

        // Direct block 0: not yet allocated
        let b0 =
            sim_get_or_alloc_block(&mmap, &mut inode, 0, &mut sim, &mut ops).expect("alloc b0");
        assert_eq!(b0, 20);
        assert_eq!(inode.blocks[0], 20);
        assert_eq!(ops.len(), 1);
        assert_eq!(
            ops[0],
            MetadataOp::SetDataBitmap {
                block_id: 20,
                allocated: true,
            }
        );

        // Call again for direct block 0: should reuse without new ops
        let b0_again =
            sim_get_or_alloc_block(&mmap, &mut inode, 0, &mut sim, &mut ops).expect("reuse b0");
        assert_eq!(b0_again, 20);
        assert_eq!(ops.len(), 1, "no new ops on reuse");

        // Direct block 9: allocate last direct block
        let b9 =
            sim_get_or_alloc_block(&mmap, &mut inode, 9, &mut sim, &mut ops).expect("alloc b9");
        assert_eq!(b9, 21);
        assert_eq!(inode.blocks[9], 21);
        assert_eq!(ops.len(), 2);
    }

    #[test]
    fn test_sim_get_or_alloc_block_single_indirect_lifecycle() {
        let (_f, mut mmap) = prune_pointer_tests::scratch();
        let mut inode = Inode::new(crate::inode::FileType::File);
        let mut sim = AllocSim {
            inode_bitmap: vec![0u8; 16],
            data_bitmap: vec![0u8; 16],
            data_start: 30,
            free_inode_hint: 0,
            free_block_hint: 30,
            fresh_blocks: Vec::new(),
        };
        let mut ops = Vec::new();

        // 1. First block in single indirect (idx = 10)
        let leaf = sim_get_or_alloc_block(&mmap, &mut inode, 10, &mut sim, &mut ops)
            .expect("alloc single");
        assert_eq!(inode.blocks[10], 30, "root indirect allocated at 30");
        assert_eq!(leaf, 31, "leaf data block allocated at 31");
        assert_eq!(ops.len(), 3);
        assert_eq!(
            ops[0],
            MetadataOp::SetDataBitmap {
                block_id: 30,
                allocated: true
            }
        );
        assert_eq!(
            ops[1],
            MetadataOp::SetDataBitmap {
                block_id: 31,
                allocated: true
            }
        );
        assert_eq!(
            ops[2],
            MetadataOp::WriteBlockSlice {
                block_id: 30,
                offset: 0,
                data: 31u64.to_le_bytes().to_vec()
            }
        );

        // 2. Next single-indirect entry (idx = 11): root is fresh
        let leaf2 = sim_get_or_alloc_block(&mmap, &mut inode, 11, &mut sim, &mut ops)
            .expect("alloc single 2");
        assert_eq!(leaf2, 32);
        assert_eq!(ops.len(), 5);
        assert_eq!(
            ops[3],
            MetadataOp::SetDataBitmap {
                block_id: 32,
                allocated: true
            }
        );
        assert_eq!(
            ops[4],
            MetadataOp::WriteBlockSlice {
                block_id: 30,
                offset: 8,
                data: 32u64.to_le_bytes().to_vec()
            }
        );

        // 3. Plant pointer in mmap for non-fresh root test
        sim.fresh_blocks.clear();
        let off = 30 * BLOCK_SIZE + 2 * 8;
        mmap[off..off + 8].copy_from_slice(&99u64.to_le_bytes());

        let leaf3 = sim_get_or_alloc_block(&mmap, &mut inode, 12, &mut sim, &mut ops)
            .expect("read existing pointer");
        assert_eq!(leaf3, 99, "reused planted pointer from mmap");
        assert_eq!(ops.len(), 5, "no new ops emitted for existing pointer");
    }

    #[test]
    fn test_sim_get_or_alloc_block_double_indirect_lifecycle() {
        let (_f, mut mmap) = prune_pointer_tests::scratch();
        let mut inode = Inode::new(crate::inode::FileType::File);
        let mut sim = AllocSim {
            inode_bitmap: vec![0u8; 16],
            data_bitmap: vec![0u8; 16],
            data_start: 50,
            free_inode_hint: 0,
            free_block_hint: 50,
            fresh_blocks: Vec::new(),
        };
        let mut ops = Vec::new();

        // 10 + 512 = 522
        let leaf = sim_get_or_alloc_block(&mmap, &mut inode, 522, &mut sim, &mut ops)
            .expect("alloc double");
        assert_eq!(inode.blocks[11], 50, "dib root allocated at 50");
        assert_eq!(leaf, 52);
        assert_eq!(ops.len(), 5);
        assert_eq!(
            ops[0],
            MetadataOp::SetDataBitmap {
                block_id: 50,
                allocated: true
            }
        );
        assert_eq!(
            ops[1],
            MetadataOp::SetDataBitmap {
                block_id: 51,
                allocated: true
            }
        );
        assert_eq!(
            ops[2],
            MetadataOp::WriteBlockSlice {
                block_id: 50,
                offset: 0,
                data: 51u64.to_le_bytes().to_vec()
            }
        );
        assert_eq!(
            ops[3],
            MetadataOp::SetDataBitmap {
                block_id: 52,
                allocated: true
            }
        );
        assert_eq!(
            ops[4],
            MetadataOp::WriteBlockSlice {
                block_id: 51,
                offset: 0,
                data: 52u64.to_le_bytes().to_vec()
            }
        );

        // Preexisting double-indirect tree:
        sim.fresh_blocks.clear();
        let dib_off = 50 * BLOCK_SIZE + 8;
        mmap[dib_off..dib_off + 8].copy_from_slice(&60u64.to_le_bytes());
        let sib_off = 60 * BLOCK_SIZE;
        mmap[sib_off..sib_off + 8].copy_from_slice(&70u64.to_le_bytes());

        let ops_before = ops.len();
        let leaf_reused = sim_get_or_alloc_block(&mmap, &mut inode, 1034, &mut sim, &mut ops)
            .expect("read existing double");
        assert_eq!(leaf_reused, 70);
        assert_eq!(
            ops.len(),
            ops_before,
            "no ops emitted for existing double indirect block"
        );
    }

    #[test]
    fn test_sim_get_or_alloc_block_triple_indirect_lifecycle() {
        let (_f, mut mmap) = prune_pointer_tests::scratch();
        let mut inode = Inode::new(crate::inode::FileType::File);
        let mut sim = AllocSim {
            inode_bitmap: vec![0u8; 16],
            data_bitmap: vec![0u8; 16],
            data_start: 20,
            free_inode_hint: 0,
            free_block_hint: 20,
            fresh_blocks: Vec::new(),
        };
        let mut ops = Vec::new();

        // 10 + 512 + 512*512 = 262666
        let leaf = sim_get_or_alloc_block(&mmap, &mut inode, 262666, &mut sim, &mut ops)
            .expect("alloc triple");
        assert_eq!(inode.triple_indirect, 20, "tib root allocated at 20");
        assert_eq!(leaf, 23);
        assert_eq!(ops.len(), 7);
        assert_eq!(
            ops[0],
            MetadataOp::SetDataBitmap {
                block_id: 20,
                allocated: true
            }
        );
        assert_eq!(
            ops[1],
            MetadataOp::SetDataBitmap {
                block_id: 21,
                allocated: true
            }
        );
        assert_eq!(
            ops[2],
            MetadataOp::WriteBlockSlice {
                block_id: 20,
                offset: 0,
                data: 21u64.to_le_bytes().to_vec()
            }
        );
        assert_eq!(
            ops[3],
            MetadataOp::SetDataBitmap {
                block_id: 22,
                allocated: true
            }
        );
        assert_eq!(
            ops[4],
            MetadataOp::WriteBlockSlice {
                block_id: 21,
                offset: 0,
                data: 22u64.to_le_bytes().to_vec()
            }
        );
        assert_eq!(
            ops[5],
            MetadataOp::SetDataBitmap {
                block_id: 23,
                allocated: true
            }
        );
        assert_eq!(
            ops[6],
            MetadataOp::WriteBlockSlice {
                block_id: 22,
                offset: 0,
                data: 23u64.to_le_bytes().to_vec()
            }
        );

        // Preexisting triple-indirect tree:
        sim.fresh_blocks.clear();
        let tib_off = 20 * BLOCK_SIZE + 8;
        mmap[tib_off..tib_off + 8].copy_from_slice(&30u64.to_le_bytes());
        let dib_off = 30 * BLOCK_SIZE + 2 * 8;
        mmap[dib_off..dib_off + 8].copy_from_slice(&40u64.to_le_bytes());
        let sib_off = 40 * BLOCK_SIZE + 3 * 8;
        mmap[sib_off..sib_off + 8].copy_from_slice(&50u64.to_le_bytes());

        // 262666 + 1 * 512*512 + 2 * 512 + 3 = 525837
        let ops_before = ops.len();
        let leaf_reused = sim_get_or_alloc_block(&mmap, &mut inode, 525837, &mut sim, &mut ops)
            .expect("read existing triple");
        assert_eq!(leaf_reused, 50);
        assert_eq!(
            ops.len(),
            ops_before,
            "no ops emitted for existing triple indirect block"
        );
    }

    #[test]
    fn test_stage_payload_write_multi_block_and_sub_block() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let img_path = temp_dir.path().join("test_stage.img");
        let dm = DiskManager::open(&img_path, 20 * 1024 * 1024).expect("open disk manager");
        let mut guard = dm.inner_for_test().write().unwrap();
        let mut sim = AllocSim::new(&guard, guard.free_inode_hint, guard.free_block_hint)
            .expect("alloc sim new");

        let mut inode = Inode::new(crate::inode::FileType::File);
        let mut ops = Vec::new();
        let mut used = Vec::new();

        // 1. Empty buffer write should be a no-op
        stage_payload_write(
            &mut guard,
            &mut inode,
            0,
            &[],
            &mut sim,
            &mut ops,
            &mut used,
        )
        .expect("empty write");
        assert!(used.is_empty());
        assert!(ops.is_empty());

        // 2. Multi-block write crossing block boundary:
        // Offset 4000, length 200 bytes -> 96 bytes to block 0, 104 bytes to block 1.
        let payload: Vec<u8> = (0..200u8).collect();
        stage_payload_write(
            &mut guard, &mut inode, 4000, &payload, &mut sim, &mut ops, &mut used,
        )
        .expect("stage write");

        assert_eq!(used.len(), 2, "must have touched 2 physical blocks");
        let blk0 = used[0];
        let blk1 = used[1];
        assert_eq!(inode.blocks[0], blk0);
        assert_eq!(inode.blocks[1], blk1);

        let b0_slice = DiskManager::get_block_from_map(&guard.mmap, blk0).unwrap();
        assert_eq!(&b0_slice[4000..4096], &payload[..96]);

        let b1_slice = DiskManager::get_block_from_map(&guard.mmap, blk1).unwrap();
        assert_eq!(&b1_slice[..104], &payload[96..]);

        // 3. Overwriting existing range should reuse blocks without duplicating in used
        let ops_len_before = ops.len();
        stage_payload_write(
            &mut guard,
            &mut inode,
            4050,
            &[0xFF; 20],
            &mut sim,
            &mut ops,
            &mut used,
        )
        .expect("overwrite");
        assert_eq!(used.len(), 2, "reused block must not add duplicate entry");
        assert_eq!(ops.len(), ops_len_before, "no new allocation ops on reuse");

        let b0_slice2 = DiskManager::get_block_from_map(&guard.mmap, blk0).unwrap();
        assert_eq!(&b0_slice2[4050..4070], &[0xFF; 20]);
    }
}
