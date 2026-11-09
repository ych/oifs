//! Bolero property / fuzz harnesses for OIFS parsers, codecs and state machines.
//!
//! Under plain `cargo test` each harness runs a bounded number of generated
//! inputs. For continuous fuzzing use e.g.
//! `cargo bolero test --test bolero_fuzz_test blockpath_roundtrip`.

use bolero::{TypeGenerator, check};
use oifs::BLOCK_SIZE;
use oifs::bitmap::Bitmap;
use oifs::directory::{
    DirectoryEntry, DirectoryIterator, find_entry_in_block, find_entry_in_block_with_hash,
    for_each_valid_inode_in_block, has_valid_entry_in_block, hash_filename,
    remove_entry_in_block_bytes, validate_filename,
};
use oifs::filters::FilterConfig;
use oifs::inode::{BlockPath, ChunkEntry, MAX_LOGICAL_BLOCKS};
use oifs::inode_format::{decode_for, decode_v2, encode_v2};
use oifs::ipc::{IpcRequest, IpcResponse, MAX_FRAME_BYTES, read_framed, write_framed};
use oifs::journal::{
    JOURNAL_RING_BLOCKS, JournalRing, JournalState, MetadataOp, apply_op_in_place, coalesce_ops,
    crc32c, decode_frame, encode_frame, encode_ops,
};
use oifs::superblock::SuperBlock;
use std::io::Cursor;

#[test]
fn blockpath_roundtrip() {
    check!()
        .with_type::<usize>()
        .for_each(|&idx| match BlockPath::from_logical(idx) {
            Some(p) => {
                assert!(idx < MAX_LOGICAL_BLOCKS);
                assert!(p.slots_in_bounds());
                assert_eq!(p.to_logical(), idx);
            }
            None => assert!(idx >= MAX_LOGICAL_BLOCKS),
        });
}

#[test]
fn chunk_entry_roundtrip() {
    check!()
        .with_type::<(u32, u8, u8, u16)>()
        .for_each(|&(sb, bc, fl, cl)| {
            let e = ChunkEntry::new(sb, bc, fl, cl);
            assert_eq!(e.start_block(), sb);
            assert_eq!(e.block_count(), bc);
            assert_eq!(e.flags(), fl);
            assert_eq!(e.compressed_len(), cl);
            assert_eq!(e.is_empty(), e.0 == 0 || fl == ChunkEntry::FLAG_EMPTY);
        });
}

#[test]
fn superblock_validate_never_panics() {
    check!()
        .with_type::<(Vec<u8>, u32)>()
        .for_each(|(bytes, mmap_blocks)| {
            let Ok(sb) = bincode::deserialize::<SuperBlock>(bytes) else {
                return;
            };
            let mmap_len = (*mmap_blocks as usize % 4096) * BLOCK_SIZE;
            if sb.validate(mmap_len).is_ok() {
                // A validated superblock must have a coherent geometry.
                assert!(sb.usable_inode_count() > 0);
                assert!(sb.inode_table_block <= sb.data_block_start);
                assert!(sb.data_block_start < sb.block_count);
                let _ = sb.journal_header_block();
                let _ = sb.has_journal_layout();
            }
        });
}

#[test]
fn superblock_new_layout_is_consistent() {
    check!()
        .with_type::<(u16, bool)>()
        .for_each(|&(blocks, journaled)| {
            let total = blocks as u64 + 64;
            if journaled && total < 128 {
                return;
            }
            let sb = SuperBlock::new_v3(total, journaled);
            assert!(sb.validate(total as usize * BLOCK_SIZE).is_ok());
            assert_eq!(sb.has_journal_layout(), journaled);
        });
}

#[test]
fn inode_format_decode_never_panics_and_roundtrips() {
    check!().with_type::<Vec<u8>>().for_each(|bytes| {
        let _ = decode_for(bytes, 1);
        if let Ok(inode) = decode_v2(bytes) {
            let slot = encode_v2(&inode);
            let again = decode_v2(&slot).expect("re-decode");
            assert_eq!(again, inode);
            assert_eq!(encode_v2(&again), slot);
        }
    });
}

#[test]
fn directory_block_parsers_never_panic() {
    check!().with_type::<Vec<u8>>().for_each(|bytes| {
        let mut block = bytes.clone();
        block.truncate(BLOCK_SIZE);
        let _ = has_valid_entry_in_block(&block);
        for_each_valid_inode_in_block(&block, |_| {});
        for r in DirectoryIterator::new(&block) {
            let _ = r;
        }
        let _ = find_entry_in_block(&block, "a");
        let _ = find_entry_in_block_with_hash(&block, "a", hash_filename("a"));

        // Removal of an entry must leave no match behind.
        if block.len() >= 19 {
            let len = u16::from_le_bytes([block[16], block[17]]) as usize;
            if len > 0
                && 18 + len <= block.len()
                && let Ok(name) = std::str::from_utf8(&block[18..18 + len])
            {
                {
                    let name = name.to_string();
                    let h = u64::from_le_bytes(block[8..16].try_into().unwrap());
                    let before = block.len();
                    if remove_entry_in_block_bytes(&mut block, &name, h).is_some() {
                        assert_eq!(block.len(), before);
                        assert_eq!(find_entry_in_block_with_hash(&block, &name, h), None);
                    }
                }
            }
        }
    });
}

#[test]
fn directory_names_serialization_roundtrip() {
    check!()
        .with_type::<(String, u64)>()
        .for_each(|(name, ino)| {
            let e = DirectoryEntry {
                inode: *ino,
                hash: hash_filename(name),
                name: name.clone(),
            };
            let mut buf = Vec::new();
            match e.serialize_into(&mut buf) {
                Ok(()) => {
                    assert!(validate_filename(name).is_ok());
                    let got = DirectoryEntry::deserialize_from(&mut Cursor::new(&buf))
                        .unwrap()
                        .unwrap();
                    assert_eq!(got, e);
                }
                Err(_) => assert!(validate_filename(name).is_err()),
            }
        });
}

fn bit(data: &[u8], i: usize) -> bool {
    data[i / 8] & (1 << (i % 8)) != 0
}

#[test]
fn bitmap_matches_naive_model() {
    check!()
        .with_type::<(Vec<u8>, u16, u16, u16)>()
        .for_each(|(data, start, count, hint)| {
            let mut data = data.clone();
            data.truncate(64);
            if data.is_empty() {
                return;
            }
            let total = data.len() * 8;
            let (start, count) = (*start as usize % (total + 8), *count as usize % (total + 8));

            // set_range / clear_range
            let mut model = data.clone();
            for i in start..(start + count).min(total) {
                model[i / 8] |= 1 << (i % 8);
            }
            let mut got = data.clone();
            Bitmap::new(&mut got).set_range(start, count);
            assert_eq!(got, model, "set_range");

            let mut model = data.clone();
            for i in start..(start + count).min(total) {
                model[i / 8] &= !(1 << (i % 8));
            }
            let mut got = data.clone();
            Bitmap::new(&mut got).clear_range(start, count);
            assert_eq!(got, model, "clear_range");

            // find_first_free_from
            let mut d = data.clone();
            let bm = Bitmap::new(&mut d);
            let naive = (start..total).find(|&i| !bit(&data, i));
            assert_eq!(
                bm.find_first_free_from(start),
                naive,
                "find_first_free_from"
            );

            // find_contiguous_free_from
            if count >= 1 && start < total {
                let naive = (start..total)
                    .find(|&s| s + count <= total && (s..s + count).all(|i| !bit(&data, i)));
                assert_eq!(
                    bm.find_contiguous_free_from(start, count),
                    naive,
                    "find_contiguous_free_from"
                );
            }

            // wrapped search returns a free bit
            if let Some(i) = bm.find_next_free_wrapped(*hint as usize) {
                assert!(!bit(&data, i));
            }
        });
}

#[test]
fn filter_pipeline_roundtrip() {
    check!()
        .with_type::<(Vec<u8>, u8, bool, bool, bool)>()
        .for_each(|(data, ts, delta, shuffle, bitshuffle)| {
            let cfg = FilterConfig {
                typesize: *ts % 17,
                delta: *delta,
                shuffle: *shuffle,
                bitshuffle: *bitshuffle,
            };
            let p = cfg.to_pipeline();
            let enc = p.apply(data);
            let mut dec = enc.clone();
            p.unapply_in_place(&mut dec);
            assert_eq!(&dec, data, "cfg={cfg:?}");
            assert_eq!(p.unapply(&enc), *data);
        });
}

#[test]
fn ipc_read_framed_never_panics() {
    check!().with_type::<Vec<u8>>().for_each(|bytes| {
        let _ = read_framed::<_, IpcRequest>(&mut Cursor::new(bytes));
        let _ = read_framed::<_, IpcResponse>(&mut Cursor::new(bytes));
    });
}

#[test]
fn ipc_frame_roundtrip() {
    check!().with_type::<Vec<u8>>().for_each(|payload| {
        let mut wire = Vec::new();
        if write_framed(&mut wire, payload).is_ok() {
            assert!(wire.len() - 4 <= MAX_FRAME_BYTES);
            let back: Vec<u8> = read_framed(&mut Cursor::new(&wire)).unwrap();
            assert_eq!(&back, payload);
        }
    });
}

#[test]
fn crc32c_matches_bytewise_reference() {
    check!().with_type::<Vec<u8>>().for_each(|data| {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in data.iter() {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0x82F6_3B78
                } else {
                    crc >> 1
                };
            }
        }
        assert_eq!(crc32c(data), !crc);
    });
}

#[test]
fn journal_decoders_never_panic() {
    check!().with_type::<Vec<u8>>().for_each(|bytes| {
        let _ = JournalState::decode(bytes);
        let _ = decode_frame(bytes);
        if let Some(ops) = MetadataOp::decode_all(bytes) {
            // Booleans are lenient on decode (any non-zero byte is `true`), so
            // the canonical re-encoding must be a fixed point of decode.
            let canon = encode_ops(&ops);
            assert_eq!(MetadataOp::decode_all(&canon), Some(ops));
        }
    });
}

#[test]
fn journal_recover_terminates_on_arbitrary_ring() {
    check!()
        .with_type::<(Vec<u8>, u64, u64)>()
        .for_each(|(noise, head, tail)| {
            let bs = BLOCK_SIZE as u64;
            let mut region = vec![0u8; ((1 + JOURNAL_RING_BLOCKS) * bs) as usize];
            let n = noise.len().min(region.len());
            region[..n].copy_from_slice(&noise[..n]);
            let st = JournalState {
                head: *head,
                tail: *tail,
                tx_seq: 0,
                cleanly_unmounted: false,
            };
            region[..40].copy_from_slice(&st.encode());
            let mut ring = JournalRing::open(&mut region, bs).expect("open");
            let _ = ring.recover(|_| Ok(()));
        });
}

#[derive(Debug, TypeGenerator)]
enum RawOp {
    Inode(u8, bool),
    Bitmap(u8, bool),
    Range(u8, u8, bool),
    Inode2(u8, u8),
    Slice(u8, u16, Vec<u8>),
}

fn build_ops(sb: &SuperBlock, raw: &[RawOp]) -> Vec<MetadataOp> {
    let span = sb.block_count - sb.data_block_start;
    let mut seen_bitmap = std::collections::HashSet::new();
    let mut ops = Vec::new();
    for r in raw.iter().take(24) {
        match r {
            RawOp::Inode(i, a) => ops.push(MetadataOp::SetInodeBitmap {
                inode_id: *i as u64 % sb.inode_count,
                allocated: *a,
            }),
            RawOp::Bitmap(b, a) => {
                let rel = *b as u64 % span;
                if seen_bitmap.insert(rel) {
                    ops.push(MetadataOp::SetDataBitmap {
                        block_id: sb.data_block_start + rel,
                        allocated: *a,
                    });
                }
            }
            RawOp::Range(b, c, a) => {
                let start = *b as u64 % span;
                let count = (*c as u64 % 8).min(span - start);
                let blocks: Vec<u64> = (start..start + count).collect();
                if blocks.iter().all(|x| !seen_bitmap.contains(x)) {
                    seen_bitmap.extend(blocks);
                    ops.push(MetadataOp::SetDataBitmapRange {
                        start_block: sb.data_block_start + start,
                        count: count as u32,
                        allocated: *a,
                    });
                }
            }
            RawOp::Inode2(i, fill) => ops.push(MetadataOp::WriteInode {
                inode_id: *i as u64 % sb.inode_count,
                inode_bytes: Box::new([*fill; 256]),
            }),
            RawOp::Slice(b, off, data) => {
                let off = *off as usize % BLOCK_SIZE;
                let mut data = data.clone();
                data.truncate(BLOCK_SIZE - off);
                ops.push(MetadataOp::WriteBlockSlice {
                    block_id: sb.data_block_start + *b as u64 % span,
                    offset: off as u32,
                    data,
                });
            }
        }
    }
    ops
}

#[test]
fn journal_coalesce_and_frame_preserve_semantics() {
    let total = 160u64;
    let sb = SuperBlock::new_v3(total, false);
    check!().with_type::<Vec<RawOp>>().for_each(|raw| {
        let ops = build_ops(&sb, raw);
        let base = vec![0u8; total as usize * BLOCK_SIZE];

        let mut a = base.clone();
        for op in &ops {
            let _ = apply_op_in_place(&mut a, &sb, op);
        }

        let mut b = base.clone();
        for op in &coalesce_ops(ops.clone()) {
            let _ = apply_op_in_place(&mut b, &sb, op);
        }
        assert!(a == b, "coalesce changed semantics: {ops:?}");

        let frame = encode_frame(7, &ops);
        let (tx, used) = decode_frame(&frame).expect("frame decodes");
        assert_eq!(used, frame.len());
        assert_eq!(tx.tx_seq, 7);
        let mut c = base;
        for op in &tx.ops {
            let _ = apply_op_in_place(&mut c, &sb, op);
        }
        assert!(a == c, "frame roundtrip changed semantics");
    });
}
