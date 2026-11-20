//! Seekable 64K Chunked compression and encryption engine for `DiskManager`:
//! - Random-access 64 KiB block-aligned chunk reads (`read_at_chunked_64k`, `read_single_chunk_into`)
//! - Out-of-lock parallel/pipelined chunk preparation (`prepare_chunk_payload`, `write_chunked_64k_pipeline`)
//! - WAL-atomic and legacy chunked writes and truncation (`commit_prepared_chunked_write`, `truncate_chunked_64k`)

use super::*;

pub(super) struct ChunkedStage1<'a> {
    initial_inode: Inode,
    new_file_size: u64,
    uncompressed_chunks: Vec<(usize, std::borrow::Cow<'a, [u8]>, usize)>,
    encryption_key: Option<crate::encryption::EncryptionKey>,
    scheme: u8,
}

impl DiskManager {
    /// Writes data to a Seekable 64K Chunked file.
    pub fn write_chunked_64k(
        &self,
        inode_id: u64,
        file_offset: u64,
        data: &[u8],
        level: i32,
    ) -> Result<(), DiskManagerError> {
        self.write_chunked_64k_pipeline(inode_id, file_offset, data, level)
    }

    #[inline]
    pub(crate) fn resolve_logical_chunk_entry(
        mmap: &MmapMut,
        inode: &Inode,
        chunk_idx: usize,
    ) -> crate::inode::ChunkEntry {
        crate::inode::ChunkEntry(Self::resolve_logical_block_id(mmap, inode, chunk_idx))
    }

    pub(super) fn set_logical_chunk_entry(
        guard: &mut DiskManagerInner,
        inode: &mut Inode,
        chunk_idx: usize,
        entry: crate::inode::ChunkEntry,
        mut ops: Option<&mut Vec<crate::journal::MetadataOp>>,
    ) -> Result<(), DiskManagerError> {
        use crate::inode::BlockPath;

        if entry.is_empty() {
            match BlockPath::from_logical(chunk_idx) {
                Some(BlockPath::Direct(i)) => {
                    inode.blocks[i] = 0;
                }
                Some(BlockPath::Single(i)) => {
                    let sib = inode.blocks[10];
                    if guard.is_data_block(sib) {
                        Self::write_block_ptr(&mut guard.mmap, sib, i, 0);
                        if let Some(ref mut o) = ops {
                            o.push(crate::journal::MetadataOp::WriteBlockSlice {
                                block_id: sib,
                                offset: (i * 8) as u32,
                                data: 0u64.to_le_bytes().to_vec(),
                            });
                        }
                    }
                }
                Some(BlockPath::Double(a, b)) => {
                    let dib = inode.blocks[11];
                    if guard.is_data_block(dib) {
                        let sib = Self::read_block_ptr(&guard.mmap, dib, a);
                        if guard.is_data_block(sib) {
                            Self::write_block_ptr(&mut guard.mmap, sib, b, 0);
                            if let Some(ref mut o) = ops {
                                o.push(crate::journal::MetadataOp::WriteBlockSlice {
                                    block_id: sib,
                                    offset: (b * 8) as u32,
                                    data: 0u64.to_le_bytes().to_vec(),
                                });
                            }
                        }
                    }
                }
                Some(BlockPath::Triple(a, b, c)) => {
                    let tib = inode.triple_indirect;
                    if guard.is_data_block(tib) {
                        let dib = Self::read_block_ptr(&guard.mmap, tib, a);
                        if guard.is_data_block(dib) {
                            let sib = Self::read_block_ptr(&guard.mmap, dib, b);
                            if guard.is_data_block(sib) {
                                Self::write_block_ptr(&mut guard.mmap, sib, c, 0);
                                if let Some(ref mut o) = ops {
                                    o.push(crate::journal::MetadataOp::WriteBlockSlice {
                                        block_id: sib,
                                        offset: (c * 8) as u32,
                                        data: 0u64.to_le_bytes().to_vec(),
                                    });
                                }
                            }
                        }
                    }
                }
                None => {}
            }
            return Ok(());
        }

        fn ensure_root(
            guard: &mut DiskManagerInner,
            slot: &mut u64,
            ops: &mut Option<&mut Vec<crate::journal::MetadataOp>>,
        ) -> Result<u64, DiskManagerError> {
            if *slot == 0 {
                let blk = DiskManager::alloc_and_zero_block(guard)?;
                *slot = blk;
                if let Some(o) = ops {
                    o.push(crate::journal::MetadataOp::SetDataBitmap {
                        block_id: blk,
                        allocated: true,
                    });
                }
            }
            // Reject forged indirect root pointers outside the data area.
            if !guard.is_data_block(*slot) {
                return Err(DiskManagerError::Io(std::io::Error::other(format!(
                    "Indirect root {} is outside the data area",
                    *slot
                ))));
            }
            Ok(*slot)
        }

        fn get_child(
            guard: &mut DiskManagerInner,
            parent_id: u64,
            entry_idx: usize,
            ops: &mut Option<&mut Vec<crate::journal::MetadataOp>>,
        ) -> Result<u64, DiskManagerError> {
            if !guard.is_data_block(parent_id) {
                return Err(DiskManagerError::Io(std::io::Error::other(format!(
                    "Indirect block {} is outside the data area",
                    parent_id
                ))));
            }
            let mut child = DiskManager::read_block_ptr(&guard.mmap, parent_id, entry_idx);
            if child != 0 && !guard.is_data_block(child) {
                return Err(DiskManagerError::Io(std::io::Error::other(format!(
                    "Indirect child {} is outside the data area",
                    child
                ))));
            }
            if child == 0 {
                child = DiskManager::alloc_and_zero_block(guard)?;
                DiskManager::write_block_ptr(&mut guard.mmap, parent_id, entry_idx, child);
                if let Some(o) = ops {
                    o.push(crate::journal::MetadataOp::SetDataBitmap {
                        block_id: child,
                        allocated: true,
                    });
                    o.push(crate::journal::MetadataOp::WriteBlockSlice {
                        block_id: parent_id,
                        offset: (entry_idx * 8) as u32,
                        data: child.to_le_bytes().to_vec(),
                    });
                }
            }
            Ok(child)
        }

        match BlockPath::from_logical(chunk_idx) {
            Some(BlockPath::Direct(i)) => {
                inode.blocks[i] = entry.0;
                Ok(())
            }
            Some(BlockPath::Single(i)) => {
                let sib = ensure_root(guard, &mut inode.blocks[10], &mut ops)?;
                Self::write_block_ptr(&mut guard.mmap, sib, i, entry.0);
                if let Some(o) = ops {
                    o.push(crate::journal::MetadataOp::WriteBlockSlice {
                        block_id: sib,
                        offset: (i * 8) as u32,
                        data: entry.0.to_le_bytes().to_vec(),
                    });
                }
                Ok(())
            }
            Some(BlockPath::Double(a, b)) => {
                let dib = ensure_root(guard, &mut inode.blocks[11], &mut ops)?;
                let sib = get_child(guard, dib, a, &mut ops)?;
                Self::write_block_ptr(&mut guard.mmap, sib, b, entry.0);
                if let Some(o) = ops {
                    o.push(crate::journal::MetadataOp::WriteBlockSlice {
                        block_id: sib,
                        offset: (b * 8) as u32,
                        data: entry.0.to_le_bytes().to_vec(),
                    });
                }
                Ok(())
            }
            Some(BlockPath::Triple(a, b, c)) => {
                let tib = ensure_root(guard, &mut inode.triple_indirect, &mut ops)?;
                let dib = get_child(guard, tib, a, &mut ops)?;
                let sib = get_child(guard, dib, b, &mut ops)?;
                Self::write_block_ptr(&mut guard.mmap, sib, c, entry.0);
                if let Some(o) = ops {
                    o.push(crate::journal::MetadataOp::WriteBlockSlice {
                        block_id: sib,
                        offset: (c * 8) as u32,
                        data: entry.0.to_le_bytes().to_vec(),
                    });
                }
                Ok(())
            }
            None => Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "Chunk index exceeds maximum addressable size",
            ))),
        }
    }

    pub(super) fn prepare_chunk_payload(
        chunk_buf: &[u8],
        chunk_valid_len: usize,
        zstd_level: i32,
        compress: bool,
        encryption_key: Option<&crate::encryption::EncryptionKey>,
        aad_ctx: ChunkAadContext,
    ) -> Result<(Vec<u8>, u8, u16, usize), DiskManagerError> {
        let compressed_opt = if compress {
            let level = if zstd_level <= 0 { 1 } else { zstd_level };
            // The pooled compressor avoids a per-chunk `CCtx` allocation.
            let compressed = crate::zstd_pool::compress_frame(&chunk_buf[..chunk_valid_len], level)
                .map_err(DiskManagerError::Io)?;
            // Check final post-AEAD size BEFORE encrypting so incompressible
            // encrypted chunks never waste an AEAD pass + nonce generation.
            let fits = if encryption_key.is_some() {
                let total_enc_len = 24 + compressed.len() + 16;
                total_enc_len < chunk_valid_len && total_enc_len <= 65535
            } else {
                compressed.len() < chunk_valid_len && compressed.len() < 65536
            };
            if fits { Some(compressed) } else { None }
        } else {
            None
        };

        if let Some(key) = encryption_key {
            let nonce = crate::encryption::generate_nonce();
            if let Some(compressed) = compressed_opt {
                // Stack-staged AAD + single-alloc `[nonce | ct | tag]`.
                let (aad_buf, aad_len) = crate::encryption::chunk_aad_for_scheme_stack(
                    aad_ctx.inode_id,
                    aad_ctx.chunk_idx,
                    crate::inode::ChunkEntry::FLAG_COMPRESSED,
                    aad_ctx.scheme,
                );
                let payload = crate::encryption::encrypt_with_nonce_prefix(
                    &compressed,
                    key,
                    &nonce,
                    &aad_buf[..aad_len],
                )?;
                let blks = payload.len().div_ceil(BLOCK_SIZE);
                let comp_len = payload.len() as u16;
                return Ok((
                    payload,
                    crate::inode::ChunkEntry::FLAG_COMPRESSED,
                    comp_len,
                    blks,
                ));
            }

            // RAW with encryption
            let (aad_buf, aad_len) = crate::encryption::chunk_aad_for_scheme_stack(
                aad_ctx.inode_id,
                aad_ctx.chunk_idx,
                crate::inode::ChunkEntry::FLAG_RAW,
                aad_ctx.scheme,
            );
            let payload = crate::encryption::encrypt_with_nonce_prefix(
                &chunk_buf[..chunk_valid_len],
                key,
                &nonce,
                &aad_buf[..aad_len],
            )?;
            let blks = payload.len().div_ceil(BLOCK_SIZE);
            let comp_len = (payload.len().min(65535)) as u16;
            Ok((payload, crate::inode::ChunkEntry::FLAG_RAW, comp_len, blks))
        } else if let Some(compressed) = compressed_opt {
            let blks = compressed.len().div_ceil(BLOCK_SIZE);
            let comp_len = compressed.len() as u16;
            Ok((
                compressed,
                crate::inode::ChunkEntry::FLAG_COMPRESSED,
                comp_len,
                blks,
            ))
        } else {
            let blks = chunk_valid_len.div_ceil(BLOCK_SIZE);
            Ok((
                chunk_buf[..chunk_valid_len].to_vec(),
                crate::inode::ChunkEntry::FLAG_RAW,
                chunk_valid_len as u16,
                blks,
            ))
        }
    }

    /// Decodes a single 64 KiB seekable chunk directly into `dst` (`dst.len() == chunk_valid_len`)
    /// with zero per-chunk heap allocations on steady-state worker threads.
    pub(super) fn read_single_chunk_into(
        guard: &DiskManagerInner,
        inode_id: u64,
        inode: &Inode,
        chunk_idx: usize,
        chunk_valid_len: usize,
        dst: &mut [u8],
    ) -> Result<(), DiskManagerError> {
        thread_local! {
            static TL_CIPHER_SCRATCH: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
            static TL_DECOMP_SCRATCH: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
        }

        debug_assert_eq!(dst.len(), chunk_valid_len);
        // On an encrypted filesystem, reject any seekable file whose inode is not
        // marked encrypted to prevent unauthenticated plaintext injection.
        if guard.superblock.encrypted && !inode.encrypted {
            return Err(DiskManagerError::DecryptionFailed);
        }
        let entry = Self::resolve_logical_chunk_entry(&guard.mmap, inode, chunk_idx);
        if entry.is_empty() {
            dst.fill(0);
            return Ok(());
        }

        let start_blk = entry.start_block() as u64;
        let blk_cnt = entry.block_count() as usize;
        let is_encrypted = inode.encrypted;
        let total_blk_bytes = blk_cnt.saturating_mul(BLOCK_SIZE);

        // A chunk's block run must lie entirely within the data area.
        if !guard.is_data_block(start_blk) {
            return Err(DiskManagerError::Io(std::io::Error::other(format!(
                "Chunk start block {} is outside the data area",
                start_blk
            ))));
        }

        if entry.is_raw() {
            let chunk_start = (chunk_idx * crate::inode::CHUNK_SIZE_64K) as u64;
            let stored_plain_len = if is_encrypted {
                (inode.size.saturating_sub(chunk_start) as usize)
                    .min(crate::inode::CHUNK_SIZE_64K)
                    .max(chunk_valid_len)
            } else {
                chunk_valid_len
            };
            let needed_bytes = if is_encrypted {
                stored_plain_len + 40
            } else {
                chunk_valid_len
            };
            let copy_len = needed_bytes.min(total_blk_bytes);
            let accessed_blks = copy_len.div_ceil(BLOCK_SIZE);
            let start_off = (start_blk as usize).checked_mul(BLOCK_SIZE);
            let end_off = start_off.and_then(|s| s.checked_add(accessed_blks * BLOCK_SIZE));
            let Some(s_off) = start_off.filter(|_| {
                end_off.is_some_and(|e| e <= guard.mmap.len())
                    && start_blk
                        .checked_add(accessed_blks as u64)
                        .is_some_and(|e| e <= guard.superblock.block_count)
            }) else {
                return Err(DiskManagerError::Io(std::io::Error::other(format!(
                    "Chunk raw block {} out of bounds",
                    start_blk
                ))));
            };
            let raw_slice = &guard.mmap[s_off..s_off + copy_len];

            if is_encrypted {
                if raw_slice.len() < 40 {
                    return Err(DiskManagerError::DecryptionFailed);
                }
                let key = guard
                    .encryption_key
                    .as_ref()
                    .ok_or(DiskManagerError::PasswordRequired)?;
                let mut nonce = [0u8; 24];
                nonce.copy_from_slice(&raw_slice[0..24]);
                let (aad_buf, aad_len) = crate::encryption::chunk_aad_for_scheme_stack(
                    inode_id,
                    chunk_idx as u64,
                    crate::inode::ChunkEntry::FLAG_RAW,
                    guard.superblock.encryption_version,
                );
                let ct_with_tag = &raw_slice[24..];
                let ct_len = ct_with_tag.len() - 16;
                if ct_len == dst.len() {
                    crate::encryption::decrypt_into_slice(
                        ct_with_tag,
                        dst,
                        key,
                        &nonce,
                        &aad_buf[..aad_len],
                    )
                    .map_err(|_| DiskManagerError::DecryptionFailed)?;
                } else {
                    TL_CIPHER_SCRATCH.with(|cell| {
                        let mut scratch = cell.borrow_mut();
                        if scratch.len() < ct_len {
                            scratch.resize(ct_len.max(crate::inode::CHUNK_SIZE_64K), 0);
                        }
                        crate::encryption::decrypt_into_slice(
                            ct_with_tag,
                            &mut scratch[..ct_len],
                            key,
                            &nonce,
                            &aad_buf[..aad_len],
                        )
                        .map_err(|_| DiskManagerError::DecryptionFailed)?;
                        let n = ct_len.min(dst.len());
                        dst[..n].copy_from_slice(&scratch[..n]);
                        if n < dst.len() {
                            dst[n..].fill(0);
                        }
                        Ok::<(), DiskManagerError>(())
                    })?;
                }
                return Ok(());
            } else {
                let n = raw_slice.len().min(dst.len());
                dst[..n].copy_from_slice(&raw_slice[..n]);
                if n < dst.len() {
                    dst[n..].fill(0);
                }
                return Ok(());
            }
        }

        if entry.is_compressed() {
            let comp_len = entry.compressed_len() as usize;
            let copy_len = comp_len.min(total_blk_bytes);
            let accessed_blks = copy_len.div_ceil(BLOCK_SIZE);
            let start_off = (start_blk as usize).checked_mul(BLOCK_SIZE);
            let end_off = start_off.and_then(|s| s.checked_add(accessed_blks * BLOCK_SIZE));
            let Some(s_off) = start_off.filter(|_| {
                end_off.is_some_and(|e| e <= guard.mmap.len())
                    && start_blk
                        .checked_add(accessed_blks as u64)
                        .is_some_and(|e| e <= guard.superblock.block_count)
            }) else {
                return Err(DiskManagerError::Io(std::io::Error::other(format!(
                    "Chunk compressed block {} out of bounds",
                    start_blk
                ))));
            };
            let comp_slice = &guard.mmap[s_off..s_off + copy_len];

            let decompress_into_dst =
                |zstd_slice: &[u8], dst: &mut [u8]| -> Result<(), DiskManagerError> {
                    if dst.len() == crate::inode::CHUNK_SIZE_64K {
                        let written = crate::zstd_pool::decompress_frame_into(zstd_slice, dst)
                            .map_err(DiskManagerError::Io)?;
                        if written < dst.len() {
                            dst[written..].fill(0);
                        }
                        Ok(())
                    } else {
                        TL_DECOMP_SCRATCH.with(|cell| {
                            let mut scratch = cell.borrow_mut();
                            if scratch.len() < crate::inode::CHUNK_SIZE_64K {
                                scratch.resize(crate::inode::CHUNK_SIZE_64K, 0);
                            }
                            let written = crate::zstd_pool::decompress_frame_into(
                                zstd_slice,
                                &mut scratch[..crate::inode::CHUNK_SIZE_64K],
                            )
                            .map_err(DiskManagerError::Io)?;
                            let n = written.min(dst.len());
                            dst[..n].copy_from_slice(&scratch[..n]);
                            if n < dst.len() {
                                dst[n..].fill(0);
                            }
                            Ok(())
                        })
                    }
                };

            if is_encrypted {
                if comp_slice.len() < 40 {
                    return Err(DiskManagerError::DecryptionFailed);
                }
                let key = guard
                    .encryption_key
                    .as_ref()
                    .ok_or(DiskManagerError::PasswordRequired)?;
                let mut nonce = [0u8; 24];
                nonce.copy_from_slice(&comp_slice[0..24]);
                let (aad_buf, aad_len) = crate::encryption::chunk_aad_for_scheme_stack(
                    inode_id,
                    chunk_idx as u64,
                    crate::inode::ChunkEntry::FLAG_COMPRESSED,
                    guard.superblock.encryption_version,
                );
                let ct_with_tag = &comp_slice[24..];
                let ct_len = ct_with_tag.len() - 16;
                return TL_CIPHER_SCRATCH.with(|cell| {
                    let mut cipher_scratch = cell.borrow_mut();
                    if cipher_scratch.len() < ct_len {
                        cipher_scratch.resize(ct_len.max(crate::inode::CHUNK_SIZE_64K), 0);
                    }
                    crate::encryption::decrypt_into_slice(
                        ct_with_tag,
                        &mut cipher_scratch[..ct_len],
                        key,
                        &nonce,
                        &aad_buf[..aad_len],
                    )
                    .map_err(|_| DiskManagerError::DecryptionFailed)?;
                    decompress_into_dst(&cipher_scratch[..ct_len], dst)
                });
            } else {
                return decompress_into_dst(comp_slice, dst);
            }
        }

        dst.fill(0);
        Ok(())
    }

    pub(super) fn read_single_chunk_decompressed(
        guard: &DiskManagerInner,
        inode_id: u64,
        inode: &Inode,
        chunk_idx: usize,
        chunk_valid_len: usize,
    ) -> Result<Vec<u8>, DiskManagerError> {
        let mut out = vec![0u8; chunk_valid_len];
        Self::read_single_chunk_into(guard, inode_id, inode, chunk_idx, chunk_valid_len, &mut out)?;
        Ok(out)
    }

    pub(super) fn read_chunked_64k_internal(
        guard: &DiskManagerInner,
        inode_id: u64,
        inode: &Inode,
    ) -> Result<Vec<u8>, DiskManagerError> {
        if inode.size == 0 {
            return Ok(Vec::new());
        }
        if guard.superblock.encrypted && !inode.encrypted {
            return Err(DiskManagerError::DecryptionFailed);
        }
        let total_size = Self::checked_read_alloc_len(inode.size, Self::MAX_READ_DATA_BYTES)?;
        let total_chunks = total_size.div_ceil(crate::inode::CHUNK_SIZE_64K);
        let mut out = Self::try_alloc_zeroed_vec(total_size)?;

        // Decode multi-chunk seekable files directly into disjoint `out` slices
        // using the persistent worker pool (warm Zstd DCtx + zero pthread_create overhead).
        if total_chunks >= 3 {
            let out_ptr = out.as_mut_ptr() as usize;
            let first_err: Mutex<Option<DiskManagerError>> = Mutex::new(None);
            let failed = std::sync::atomic::AtomicBool::new(false);
            crate::zstd_pool::par_for_index(total_chunks, |chunk_idx| {
                if failed.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                let start = chunk_idx * crate::inode::CHUNK_SIZE_64K;
                let len = (total_size - start).min(crate::inode::CHUNK_SIZE_64K);
                // SAFETY: each `chunk_idx` owns the disjoint range `[start..start + len)` within `out`.
                let dst =
                    unsafe { std::slice::from_raw_parts_mut((out_ptr as *mut u8).add(start), len) };
                if let Err(e) =
                    Self::read_single_chunk_into(guard, inode_id, inode, chunk_idx, len, dst)
                {
                    failed.store(true, std::sync::atomic::Ordering::Relaxed);
                    let mut err_slot = crate::lock_unpoisoned(&first_err);
                    if err_slot.is_none() {
                        *err_slot = Some(e);
                    }
                }
            });
            if let Some(e) = first_err.into_inner().unwrap_or_else(|e| e.into_inner()) {
                return Err(e);
            }
        } else {
            for (chunk_idx, dst) in out.chunks_mut(crate::inode::CHUNK_SIZE_64K).enumerate() {
                Self::read_single_chunk_into(guard, inode_id, inode, chunk_idx, dst.len(), dst)?;
            }
        }

        Ok(out)
    }

    pub(super) fn read_at_chunked_64k(
        guard: &DiskManagerInner,
        inode_id: u64,
        inode: &Inode,
        file_offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, DiskManagerError> {
        if file_offset >= inode.size || buf.is_empty() {
            return Ok(0);
        }

        let to_read_total = (buf.len()).min((inode.size - file_offset) as usize);

        // Parallel multi-chunk fast path for chunk-aligned `read_at` calls (>= 192 KiB)
        // using the persistent worker pool (warm Zstd DCtx + zero pthread_create overhead).
        if (file_offset as usize).is_multiple_of(crate::inode::CHUNK_SIZE_64K)
            && to_read_total >= 3 * crate::inode::CHUNK_SIZE_64K
        {
            let base_chunk = (file_offset as usize) / crate::inode::CHUNK_SIZE_64K;
            let total_chunks = to_read_total.div_ceil(crate::inode::CHUNK_SIZE_64K);
            let buf_ptr = buf.as_mut_ptr() as usize;
            let first_err: Mutex<Option<DiskManagerError>> = Mutex::new(None);
            let failed = std::sync::atomic::AtomicBool::new(false);
            crate::zstd_pool::par_for_index(total_chunks, |rel_idx| {
                if failed.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                let start = rel_idx * crate::inode::CHUNK_SIZE_64K;
                let len = (to_read_total - start).min(crate::inode::CHUNK_SIZE_64K);
                // SAFETY: each `rel_idx` owns the disjoint range `[start..start + len)` within `buf`.
                let dst =
                    unsafe { std::slice::from_raw_parts_mut((buf_ptr as *mut u8).add(start), len) };
                let chunk_idx = base_chunk + rel_idx;
                let chunk_start = (chunk_idx * crate::inode::CHUNK_SIZE_64K) as u64;
                let chunk_valid_len =
                    ((inode.size - chunk_start) as usize).min(crate::inode::CHUNK_SIZE_64K);
                let res = if len == chunk_valid_len {
                    Self::read_single_chunk_into(
                        guard,
                        inode_id,
                        inode,
                        chunk_idx,
                        chunk_valid_len,
                        dst,
                    )
                } else {
                    Self::read_single_chunk_decompressed(
                        guard,
                        inode_id,
                        inode,
                        chunk_idx,
                        chunk_valid_len,
                    )
                    .map(|tmp| dst.copy_from_slice(&tmp[..len]))
                };
                if let Err(e) = res {
                    failed.store(true, std::sync::atomic::Ordering::Relaxed);
                    let mut err_slot = crate::lock_unpoisoned(&first_err);
                    if err_slot.is_none() {
                        *err_slot = Some(e);
                    }
                }
            });
            if let Some(e) = first_err.into_inner().unwrap_or_else(|e| e.into_inner()) {
                return Err(e);
            }
            return Ok(to_read_total);
        }

        let mut bytes_read = 0;
        let mut curr_offset = file_offset;

        while bytes_read < to_read_total {
            let chunk_idx = (curr_offset as usize) / crate::inode::CHUNK_SIZE_64K;
            let in_chunk_offset = (curr_offset as usize) % crate::inode::CHUNK_SIZE_64K;
            let chunk_start = (chunk_idx * crate::inode::CHUNK_SIZE_64K) as u64;
            let chunk_valid_len =
                ((inode.size - chunk_start) as usize).min(crate::inode::CHUNK_SIZE_64K);

            let rem_in_chunk = chunk_valid_len.saturating_sub(in_chunk_offset);
            if rem_in_chunk == 0 {
                break;
            }
            let chunk_to_copy = (to_read_total - bytes_read).min(rem_in_chunk);

            // Full valid chunk: decode straight into caller's `buf` (zero allocation, zero cache lock).
            if in_chunk_offset == 0 && chunk_to_copy == chunk_valid_len {
                Self::read_single_chunk_into(
                    guard,
                    inode_id,
                    inode,
                    chunk_idx,
                    chunk_valid_len,
                    &mut buf[bytes_read..bytes_read + chunk_valid_len],
                )?;
                bytes_read += chunk_valid_len;
                curr_offset += chunk_valid_len as u64;
                continue;
            }

            // Sub-chunk slice: serve from bounded 8-slot `ChunkReadCache`.
            let raw_entry = Self::resolve_logical_chunk_entry(&guard.mmap, inode, chunk_idx).0;
            if raw_entry == 0 {
                buf[bytes_read..bytes_read + chunk_to_copy].fill(0);
                bytes_read += chunk_to_copy;
                curr_offset += chunk_to_copy as u64;
                continue;
            }

            // Fast path: copy directly under shared read lock (zero Arc clone/drop atomic traffic).
            let (hit_len, is_sequential) = {
                let cache = guard
                    .chunk_read_cache
                    .read()
                    .unwrap_or_else(|e| e.into_inner());
                let hit = cache.copy_slice_if_hit(
                    inode_id,
                    chunk_idx,
                    raw_entry,
                    chunk_valid_len,
                    in_chunk_offset,
                    &mut buf[bytes_read..bytes_read + chunk_to_copy],
                );
                let seq = hit.is_none()
                    && in_chunk_offset == 0
                    && chunk_idx > 0
                    && cache.has_chunk(inode_id, chunk_idx - 1);
                (hit, seq)
            };
            let available = match hit_len {
                Some(n) => n,
                None => {
                    let total_file_chunks =
                        (inode.size as usize).div_ceil(crate::inode::CHUNK_SIZE_64K);
                    let prefetch_count = if is_sequential {
                        (total_file_chunks - chunk_idx).min(4)
                    } else {
                        1
                    };

                    if prefetch_count > 1 {
                        struct SyncPrefetch<T>(Vec<std::cell::UnsafeCell<Option<T>>>);
                        // SAFETY: `par_for_index` visits each index `0..prefetch_count` once.
                        unsafe impl<T: Send> Sync for SyncPrefetch<T> {}
                        impl<T> SyncPrefetch<T> {
                            #[inline]
                            unsafe fn set(&self, i: usize, val: T) {
                                unsafe {
                                    *self.0[i].get() = Some(val);
                                }
                            }
                        }
                        let mut targets: Vec<(usize, u64, usize)> =
                            Vec::with_capacity(prefetch_count);
                        for k in 0..prefetch_count {
                            let c_idx = chunk_idx + k;
                            let c_entry =
                                Self::resolve_logical_chunk_entry(&guard.mmap, inode, c_idx).0;
                            if c_entry == 0 {
                                break;
                            }
                            let c_start = (c_idx * crate::inode::CHUNK_SIZE_64K) as u64;
                            let c_valid =
                                ((inode.size - c_start) as usize).min(crate::inode::CHUNK_SIZE_64K);
                            targets.push((c_idx, c_entry, c_valid));
                        }
                        let n_targets = targets.len();
                        let slots = SyncPrefetch(
                            (0..n_targets)
                                .map(|_| std::cell::UnsafeCell::new(None))
                                .collect(),
                        );
                        crate::zstd_pool::par_for_index(n_targets, |k| {
                            let (c_idx, _, c_valid) = targets[k];
                            let mut tmp = vec![0u8; c_valid];
                            let res = Self::read_single_chunk_into(
                                guard, inode_id, inode, c_idx, c_valid, &mut tmp,
                            )
                            .map(|()| tmp);
                            // SAFETY: index `k` is uniquely owned by this invocation.
                            unsafe {
                                slots.set(k, res);
                            }
                        });
                        let mut decoded: Vec<Vec<u8>> = Vec::with_capacity(n_targets);
                        for cell in slots.0 {
                            decoded.push(cell.into_inner().expect("prefetch slot populated")?);
                        }
                        let avail = decoded[0]
                            .len()
                            .saturating_sub(in_chunk_offset)
                            .min(chunk_to_copy);
                        if avail > 0 {
                            buf[bytes_read..bytes_read + avail].copy_from_slice(
                                &decoded[0][in_chunk_offset..in_chunk_offset + avail],
                            );
                        }
                        let mut cache = guard
                            .chunk_read_cache
                            .write()
                            .unwrap_or_else(|e| e.into_inner());
                        for ((c_idx, c_entry, c_valid), tmp) in targets.into_iter().zip(decoded) {
                            cache.insert(inode_id, c_idx, c_entry, c_valid, Arc::from(tmp));
                        }
                        avail
                    } else {
                        let mut tmp = vec![0u8; chunk_valid_len];
                        Self::read_single_chunk_into(
                            guard,
                            inode_id,
                            inode,
                            chunk_idx,
                            chunk_valid_len,
                            &mut tmp,
                        )?;
                        let avail = tmp.len().saturating_sub(in_chunk_offset).min(chunk_to_copy);
                        if avail > 0 {
                            buf[bytes_read..bytes_read + avail]
                                .copy_from_slice(&tmp[in_chunk_offset..in_chunk_offset + avail]);
                        }
                        let arc: Arc<[u8]> = Arc::from(tmp);
                        guard
                            .chunk_read_cache
                            .write()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(inode_id, chunk_idx, raw_entry, chunk_valid_len, arc);
                        avail
                    }
                }
            };
            if available == 0 {
                break;
            }
            bytes_read += available;
            curr_offset += available as u64;
        }

        Ok(bytes_read)
    }

    pub(super) fn write_chunked_64k_pipeline(
        &self,
        inode_id: u64,
        file_offset: u64,
        data: &[u8],
        level: i32,
    ) -> Result<(), DiskManagerError> {
        self.write_chunked_64k_pipeline_with_mode(inode_id, file_offset, data, level, true)
    }

    /// 3-Stage Out-of-Lock Parallel Pipeline for Seekable 64K Chunked writes.
    ///
    /// 1. Stage 1 (Read lock): Inspects inode, borrows full-chunk slices directly (`Cow::Borrowed`),
    ///    and prepares uncompressed 64KB chunk buffers only for RMW or format-upgraded chunks.
    /// 2. Stage 2 (Zero locks): Parallel Zstd compression and XChaCha20 encryption across CPU cores.
    /// 3. Stage 3 (Write lock): Allocates blocks and commits WAL transaction via AllocSim, followed by idempotent apply.
    pub(super) fn write_chunked_64k_pipeline_with_mode(
        &self,
        inode_id: u64,
        file_offset: u64,
        data: &[u8],
        level: i32,
        compress: bool,
    ) -> Result<(), DiskManagerError> {
        if data.is_empty() {
            return Ok(());
        }

        // --- STAGE 1: Fast Shared Read-Lock Inspection (Out-of-Lock Preparation) ---
        let stage1 = {
            let guard = self.inner.read().unwrap();
            let inode = Self::read_inode_internal(&guard, inode_id)?;
            if inode.mode != crate::inode::FileType::File {
                return Err(DiskManagerError::Io(std::io::Error::other(
                    "Cannot write chunked data to non-file inode",
                )));
            }
            Self::prepare_chunked_64k_stage1(&guard, inode_id, inode, file_offset, data)?
        };

        self.finish_chunked_64k_pipeline(inode_id, file_offset, data, level, compress, stage1)
    }

    pub(super) fn prepare_chunked_64k_stage1<'a>(
        guard: &DiskManagerInner,
        inode_id: u64,
        inode: Inode,
        file_offset: u64,
        data: &'a [u8],
    ) -> Result<ChunkedStage1<'a>, DiskManagerError> {
        let was_seekable = (inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K) != 0;
        let first_chunk = (file_offset as usize) / crate::inode::CHUNK_SIZE_64K;
        let last_chunk =
            ((file_offset + data.len() as u64 - 1) as usize) / crate::inode::CHUNK_SIZE_64K;

        let (stage1_new_file_size, uncompressed_chunks) = if was_seekable || inode.size == 0 {
            let old_logical_size = if was_seekable { inode.size } else { 0 };
            let new_file_size = old_logical_size.max(file_offset + data.len() as u64);
            let mut chunks: Vec<(usize, std::borrow::Cow<'a, [u8]>, usize)> =
                Vec::with_capacity(last_chunk - first_chunk + 1);

            for chunk_idx in first_chunk..=last_chunk {
                let chunk_file_start = (chunk_idx * crate::inode::CHUNK_SIZE_64K) as u64;
                let chunk_file_end = chunk_file_start + crate::inode::CHUNK_SIZE_64K as u64;

                let write_file_start = file_offset.max(chunk_file_start);
                let write_file_end = (file_offset + data.len() as u64).min(chunk_file_end);
                let data_start = (write_file_start - file_offset) as usize;
                let data_end = (write_file_end - file_offset) as usize;
                let chunk_offset_start = (write_file_start - chunk_file_start) as usize;
                let chunk_offset_end = (write_file_end - chunk_file_start) as usize;

                let chunk_valid_len =
                    ((new_file_size - chunk_file_start) as usize).min(crate::inode::CHUNK_SIZE_64K);

                let chunk_cow = if chunk_offset_start == 0 && chunk_offset_end == chunk_valid_len {
                    std::borrow::Cow::Borrowed(&data[data_start..data_end])
                } else {
                    let old_valid_len = if was_seekable && chunk_file_start < inode.size {
                        ((inode.size - chunk_file_start) as usize).min(crate::inode::CHUNK_SIZE_64K)
                    } else {
                        0
                    };
                    let mut buf = vec![0u8; chunk_valid_len];
                    if old_valid_len > 0 {
                        let read_len = old_valid_len.min(chunk_valid_len);
                        let raw_entry =
                            Self::resolve_logical_chunk_entry(&guard.mmap, &inode, chunk_idx).0;
                        let cache_hit = raw_entry != 0
                            && guard
                                .chunk_read_cache
                                .read()
                                .unwrap_or_else(|e| e.into_inner())
                                .copy_slice_if_hit(
                                    inode_id,
                                    chunk_idx,
                                    raw_entry,
                                    read_len,
                                    0,
                                    &mut buf[..read_len],
                                )
                                == Some(read_len);
                        if !cache_hit {
                            Self::read_single_chunk_into(
                                guard,
                                inode_id,
                                &inode,
                                chunk_idx,
                                read_len,
                                &mut buf[..read_len],
                            )?;
                        }
                    }
                    buf[chunk_offset_start..chunk_offset_end]
                        .copy_from_slice(&data[data_start..data_end]);
                    std::borrow::Cow::Owned(buf)
                };

                chunks.push((chunk_idx, chunk_cow, chunk_valid_len));
            }
            (new_file_size, chunks)
        } else {
            // One-time transparent upgrade from whole-file blob to 64 KiB seekable chunks.
            let old_data = Self::read_data_internal(guard, inode_id, &inode)?;
            let old_logical_size = old_data.len() as u64;
            let new_file_size = old_logical_size.max(file_offset + data.len() as u64);
            let old_chunks = old_data.len().div_ceil(crate::inode::CHUNK_SIZE_64K);

            let prefix_end = old_chunks.min(first_chunk);
            let suffix_start = (last_chunk + 1).max(prefix_end);
            let suffix_count = old_chunks.saturating_sub(suffix_start);
            let mut chunks: Vec<(usize, std::borrow::Cow<'a, [u8]>, usize)> =
                Vec::with_capacity(prefix_end + (last_chunk - first_chunk + 1) + suffix_count);

            // 1. Untouched prefix chunks from `old_data`
            for chunk_idx in 0..prefix_end {
                let c_start = chunk_idx * crate::inode::CHUNK_SIZE_64K;
                let c_old_end = (c_start + crate::inode::CHUNK_SIZE_64K).min(old_data.len());
                let chunk_file_start = c_start as u64;
                let chunk_valid_len =
                    ((new_file_size - chunk_file_start) as usize).min(crate::inode::CHUNK_SIZE_64K);
                let mut buf = vec![0u8; chunk_valid_len];
                let copy_n = (c_old_end - c_start).min(chunk_valid_len);
                buf[..copy_n].copy_from_slice(&old_data[c_start..c_start + copy_n]);
                chunks.push((chunk_idx, std::borrow::Cow::Owned(buf), chunk_valid_len));
            }

            // 2. Written range `first_chunk..=last_chunk`
            for chunk_idx in first_chunk..=last_chunk {
                let chunk_file_start = (chunk_idx * crate::inode::CHUNK_SIZE_64K) as u64;
                let chunk_file_end = chunk_file_start + crate::inode::CHUNK_SIZE_64K as u64;
                let write_file_start = file_offset.max(chunk_file_start);
                let write_file_end = (file_offset + data.len() as u64).min(chunk_file_end);
                let data_start = (write_file_start - file_offset) as usize;
                let data_end = (write_file_end - file_offset) as usize;
                let chunk_offset_start = (write_file_start - chunk_file_start) as usize;
                let chunk_offset_end = (write_file_end - chunk_file_start) as usize;
                let chunk_valid_len =
                    ((new_file_size - chunk_file_start) as usize).min(crate::inode::CHUNK_SIZE_64K);

                if chunk_offset_start == 0 && chunk_offset_end == chunk_valid_len {
                    chunks.push((
                        chunk_idx,
                        std::borrow::Cow::Borrowed(&data[data_start..data_end]),
                        chunk_valid_len,
                    ));
                } else {
                    let mut buf = vec![0u8; chunk_valid_len];
                    let c_start = chunk_idx * crate::inode::CHUNK_SIZE_64K;
                    if c_start < old_data.len() {
                        let c_old_end =
                            (c_start + crate::inode::CHUNK_SIZE_64K).min(old_data.len());
                        let copy_n = (c_old_end - c_start).min(chunk_valid_len);
                        buf[..copy_n].copy_from_slice(&old_data[c_start..c_start + copy_n]);
                    }
                    buf[chunk_offset_start..chunk_offset_end]
                        .copy_from_slice(&data[data_start..data_end]);
                    chunks.push((chunk_idx, std::borrow::Cow::Owned(buf), chunk_valid_len));
                }
            }

            // 3. Untouched suffix chunks from `old_data`
            for chunk_idx in suffix_start..old_chunks {
                let c_start = chunk_idx * crate::inode::CHUNK_SIZE_64K;
                let c_old_end = (c_start + crate::inode::CHUNK_SIZE_64K).min(old_data.len());
                let chunk_valid_len = c_old_end - c_start;
                chunks.push((
                    chunk_idx,
                    std::borrow::Cow::Owned(old_data[c_start..c_old_end].to_vec()),
                    chunk_valid_len,
                ));
            }
            (new_file_size, chunks)
        };

        Ok(ChunkedStage1 {
            initial_inode: inode,
            new_file_size: stage1_new_file_size,
            uncompressed_chunks,
            encryption_key: guard.encryption_key.clone(),
            scheme: guard.superblock.encryption_version,
        })
    }

    pub(super) fn finish_chunked_64k_pipeline(
        &self,
        inode_id: u64,
        file_offset: u64,
        data: &[u8],
        level: i32,
        compress: bool,
        stage1: ChunkedStage1<'_>,
    ) -> Result<(), DiskManagerError> {
        let ChunkedStage1 {
            initial_inode,
            new_file_size: stage1_new_file_size,
            uncompressed_chunks,
            encryption_key,
            scheme,
        } = stage1;

        // --- STAGE 2: CPU-Bound Processing (ZERO filesystem locks held) ---
        let total_chunks = uncompressed_chunks.len();
        let prepared_chunks: Vec<PreparedChunkPayload> = if total_chunks > 1 {
            let enc_key_ref = encryption_key.as_ref();
            let slots: Vec<
                std::cell::UnsafeCell<Option<Result<PreparedChunkPayload, DiskManagerError>>>,
            > = (0..total_chunks)
                .map(|_| std::cell::UnsafeCell::new(None))
                .collect();
            struct SyncSlots<T>(Vec<std::cell::UnsafeCell<Option<T>>>);
            // SAFETY: `par_for_index` guarantees each index `i in 0..total_chunks` is
            // executed at most once, so every thread writes exclusively to `slots[i]`.
            unsafe impl<T: Send> Sync for SyncSlots<T> {}
            impl<T> SyncSlots<T> {
                #[inline]
                unsafe fn set(&self, i: usize, val: T) {
                    unsafe {
                        *self.0[i].get() = Some(val);
                    }
                }
            }
            let sync_slots = SyncSlots(slots);

            crate::zstd_pool::par_for_index(total_chunks, |i| {
                let (chunk_idx, chunk_buf, chunk_valid_len) = &uncompressed_chunks[i];
                let res = Self::prepare_chunk_payload(
                    chunk_buf.as_ref(),
                    *chunk_valid_len,
                    level,
                    compress,
                    enc_key_ref,
                    ChunkAadContext {
                        inode_id,
                        chunk_idx: *chunk_idx as u64,
                        scheme,
                    },
                )
                .map(|(payload, flag, comp_len, blocks_needed)| {
                    PreparedChunkPayload {
                        chunk_idx: *chunk_idx,
                        payload,
                        flag,
                        comp_len,
                        blocks_needed,
                    }
                });
                // SAFETY: index `i` is uniquely owned by this invocation.
                unsafe {
                    sync_slots.set(i, res);
                }
            });

            let mut all = Vec::with_capacity(total_chunks);
            for cell in sync_slots.0 {
                all.push(cell.into_inner().expect("every chunk slot populated")?);
            }
            all
        } else {
            let mut out = Vec::with_capacity(total_chunks);
            for (chunk_idx, chunk_buf, chunk_valid_len) in uncompressed_chunks {
                let (payload, flag, comp_len, blocks_needed) = Self::prepare_chunk_payload(
                    chunk_buf.as_ref(),
                    chunk_valid_len,
                    level,
                    compress,
                    encryption_key.as_ref(),
                    ChunkAadContext {
                        inode_id,
                        chunk_idx: chunk_idx as u64,
                        scheme,
                    },
                )?;
                out.push(PreparedChunkPayload {
                    chunk_idx,
                    payload,
                    flag,
                    comp_len,
                    blocks_needed,
                });
            }
            out
        };

        // --- STAGE 3: Exclusive Lock Acquisition & Commit ---
        let mut guard = self.inner.write().unwrap();
        let inode = Self::read_inode_internal(&guard, inode_id)?;
        if inode.mode != crate::inode::FileType::File {
            return Err(DiskManagerError::Io(std::io::Error::other(
                "Cannot write chunked data to non-file inode",
            )));
        }

        let can_use_prepared = inode.size == initial_inode.size
            && inode.compressed_size == initial_inode.compressed_size
            && inode.modified_at == initial_inode.modified_at
            && inode.flags == initial_inode.flags
            && inode.blocks == initial_inode.blocks
            && inode.triple_indirect == initial_inode.triple_indirect;

        let res = if !can_use_prepared {
            Self::write_chunked_64k_internal(
                &mut guard,
                inode_id,
                file_offset,
                data,
                level,
                compress,
            )
        } else {
            Self::commit_prepared_chunked_write(
                &mut guard,
                inode_id,
                stage1_new_file_size,
                prepared_chunks,
            )
        };
        drop(guard);
        self.maybe_drain_async_ranges();
        res
    }

    pub(super) fn write_chunked_64k_internal(
        guard: &mut DiskManagerInner,
        inode_id: u64,
        file_offset: u64,
        data: &[u8],
        zstd_level: i32,
        compress: bool,
    ) -> Result<(), DiskManagerError> {
        let inode = Self::read_inode_internal(guard, inode_id)?;
        if inode.mode != crate::inode::FileType::File {
            return Err(DiskManagerError::Io(std::io::Error::other(
                "Cannot write chunked data to non-file inode",
            )));
        }

        if data.is_empty() {
            return Ok(());
        }

        let was_seekable = (inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K) != 0;
        let first_chunk = (file_offset as usize) / crate::inode::CHUNK_SIZE_64K;
        let last_chunk =
            ((file_offset + data.len() as u64 - 1) as usize) / crate::inode::CHUNK_SIZE_64K;

        if was_seekable || inode.size == 0 {
            let old_logical_size = if was_seekable { inode.size } else { 0 };
            let new_file_size = old_logical_size.max(file_offset + data.len() as u64);
            let mut prepared_chunks = Vec::with_capacity(last_chunk - first_chunk + 1);

            for chunk_idx in first_chunk..=last_chunk {
                let chunk_file_start = (chunk_idx * crate::inode::CHUNK_SIZE_64K) as u64;
                let chunk_file_end = chunk_file_start + crate::inode::CHUNK_SIZE_64K as u64;

                let write_file_start = file_offset.max(chunk_file_start);
                let write_file_end = (file_offset + data.len() as u64).min(chunk_file_end);
                let data_start = (write_file_start - file_offset) as usize;
                let data_end = (write_file_end - file_offset) as usize;
                let chunk_offset_start = (write_file_start - chunk_file_start) as usize;
                let chunk_offset_end = (write_file_end - chunk_file_start) as usize;

                let chunk_valid_len =
                    ((new_file_size - chunk_file_start) as usize).min(crate::inode::CHUNK_SIZE_64K);

                let owned_buf;
                let chunk_slice: &[u8] = if chunk_offset_start == 0
                    && chunk_offset_end == chunk_valid_len
                {
                    &data[data_start..data_end]
                } else {
                    let old_valid_len = if was_seekable && chunk_file_start < inode.size {
                        ((inode.size - chunk_file_start) as usize).min(crate::inode::CHUNK_SIZE_64K)
                    } else {
                        0
                    };
                    let mut buf = vec![0u8; chunk_valid_len];
                    if old_valid_len > 0 {
                        let read_len = old_valid_len.min(chunk_valid_len);
                        Self::read_single_chunk_into(
                            guard,
                            inode_id,
                            &inode,
                            chunk_idx,
                            read_len,
                            &mut buf[..read_len],
                        )?;
                    }
                    buf[chunk_offset_start..chunk_offset_end]
                        .copy_from_slice(&data[data_start..data_end]);
                    owned_buf = buf;
                    &owned_buf
                };

                let (payload, flag, comp_len, blocks_needed) = Self::prepare_chunk_payload(
                    chunk_slice,
                    chunk_valid_len,
                    zstd_level,
                    compress,
                    guard.encryption_key.as_ref(),
                    ChunkAadContext {
                        inode_id,
                        chunk_idx: chunk_idx as u64,
                        scheme: guard.superblock.encryption_version,
                    },
                )?;

                prepared_chunks.push(PreparedChunkPayload {
                    chunk_idx,
                    payload,
                    flag,
                    comp_len,
                    blocks_needed,
                });
            }

            Self::commit_prepared_chunked_write(guard, inode_id, new_file_size, prepared_chunks)
        } else {
            let old_data = Self::read_data_internal(guard, inode_id, &inode)?;
            let old_logical_size = old_data.len() as u64;
            let new_file_size = old_logical_size.max(file_offset + data.len() as u64);
            let old_chunks = old_data.len().div_ceil(crate::inode::CHUNK_SIZE_64K);

            let prefix_end = old_chunks.min(first_chunk);
            let suffix_start = (last_chunk + 1).max(prefix_end);
            let suffix_count = old_chunks.saturating_sub(suffix_start);
            let mut prepared_chunks =
                Vec::with_capacity(prefix_end + (last_chunk - first_chunk + 1) + suffix_count);

            for chunk_idx in 0..prefix_end {
                let c_start = chunk_idx * crate::inode::CHUNK_SIZE_64K;
                let c_old_end = (c_start + crate::inode::CHUNK_SIZE_64K).min(old_data.len());
                let chunk_file_start = c_start as u64;
                let chunk_valid_len =
                    ((new_file_size - chunk_file_start) as usize).min(crate::inode::CHUNK_SIZE_64K);
                let mut buf = vec![0u8; chunk_valid_len];
                let copy_n = (c_old_end - c_start).min(chunk_valid_len);
                buf[..copy_n].copy_from_slice(&old_data[c_start..c_start + copy_n]);
                let (payload, flag, comp_len, blocks_needed) = Self::prepare_chunk_payload(
                    &buf,
                    chunk_valid_len,
                    zstd_level,
                    compress,
                    guard.encryption_key.as_ref(),
                    ChunkAadContext {
                        inode_id,
                        chunk_idx: chunk_idx as u64,
                        scheme: guard.superblock.encryption_version,
                    },
                )?;
                prepared_chunks.push(PreparedChunkPayload {
                    chunk_idx,
                    payload,
                    flag,
                    comp_len,
                    blocks_needed,
                });
            }

            for chunk_idx in first_chunk..=last_chunk {
                let chunk_file_start = (chunk_idx * crate::inode::CHUNK_SIZE_64K) as u64;
                let chunk_file_end = chunk_file_start + crate::inode::CHUNK_SIZE_64K as u64;
                let write_file_start = file_offset.max(chunk_file_start);
                let write_file_end = (file_offset + data.len() as u64).min(chunk_file_end);
                let data_start = (write_file_start - file_offset) as usize;
                let data_end = (write_file_end - file_offset) as usize;
                let chunk_offset_start = (write_file_start - chunk_file_start) as usize;
                let chunk_offset_end = (write_file_end - chunk_file_start) as usize;
                let chunk_valid_len =
                    ((new_file_size - chunk_file_start) as usize).min(crate::inode::CHUNK_SIZE_64K);

                let mut buf = vec![0u8; chunk_valid_len];
                let c_start = chunk_idx * crate::inode::CHUNK_SIZE_64K;
                if c_start < old_data.len() {
                    let c_old_end = (c_start + crate::inode::CHUNK_SIZE_64K).min(old_data.len());
                    let copy_n = (c_old_end - c_start).min(chunk_valid_len);
                    buf[..copy_n].copy_from_slice(&old_data[c_start..c_start + copy_n]);
                }
                buf[chunk_offset_start..chunk_offset_end]
                    .copy_from_slice(&data[data_start..data_end]);
                let (payload, flag, comp_len, blocks_needed) = Self::prepare_chunk_payload(
                    &buf,
                    chunk_valid_len,
                    zstd_level,
                    compress,
                    guard.encryption_key.as_ref(),
                    ChunkAadContext {
                        inode_id,
                        chunk_idx: chunk_idx as u64,
                        scheme: guard.superblock.encryption_version,
                    },
                )?;
                prepared_chunks.push(PreparedChunkPayload {
                    chunk_idx,
                    payload,
                    flag,
                    comp_len,
                    blocks_needed,
                });
            }

            for chunk_idx in suffix_start..old_chunks {
                let c_start = chunk_idx * crate::inode::CHUNK_SIZE_64K;
                let c_old_end = (c_start + crate::inode::CHUNK_SIZE_64K).min(old_data.len());
                let chunk_valid_len = c_old_end - c_start;
                let (payload, flag, comp_len, blocks_needed) = Self::prepare_chunk_payload(
                    &old_data[c_start..c_old_end],
                    chunk_valid_len,
                    zstd_level,
                    compress,
                    guard.encryption_key.as_ref(),
                    ChunkAadContext {
                        inode_id,
                        chunk_idx: chunk_idx as u64,
                        scheme: guard.superblock.encryption_version,
                    },
                )?;
                prepared_chunks.push(PreparedChunkPayload {
                    chunk_idx,
                    payload,
                    flag,
                    comp_len,
                    blocks_needed,
                });
            }

            Self::commit_prepared_chunked_write(guard, inode_id, new_file_size, prepared_chunks)
        }
    }

    pub(super) fn commit_prepared_chunked_write(
        guard: &mut DiskManagerInner,
        inode_id: u64,
        new_file_size: u64,
        prepared_chunks: Vec<PreparedChunkPayload>,
    ) -> Result<(), DiskManagerError> {
        let mut inode = Self::read_inode_internal(guard, inode_id)?;
        let was_seekable = (inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K) != 0;
        let old_non_seekable_blocks = if !was_seekable {
            Self::collect_inode_blocks(&guard.mmap, &inode)
        } else {
            Vec::new()
        };
        if !was_seekable {
            inode.blocks = [0; 12];
            inode.triple_indirect = 0;
            inode.compressed_size = 0;
        }
        inode.flags |= crate::inode::INODE_FLAG_SEEKABLE_64K;
        if guard.encryption_key.is_some() {
            inode.encrypted = true;
            if inode.encryption_nonce == [0u8; 24] {
                inode.encryption_nonce = crate::encryption::generate_nonce();
            }
        }

        if guard.superblock.has_journal_layout() {
            let mut sim = crate::journal::AllocSim::new_data_only(
                guard,
                guard.free_inode_hint,
                guard.free_block_hint,
            )?;
            let mut journal_ops: Vec<crate::journal::MetadataOp> = Vec::new();
            let mut payload_ranges: Vec<(usize, usize)> = Vec::new();
            let mut compressed_size_delta: i64 = 0;

            for prepared in prepared_chunks {
                // 1. Allocate blocks via AllocSim (simulated in memory, NOT touching mmap data bitmap)
                let new_start_blk = sim.alloc_contiguous(&guard.mmap, prepared.blocks_needed)?;
                crate::journal::push_data_bitmap_op(
                    &mut journal_ops,
                    new_start_blk,
                    prepared.blocks_needed as u32,
                    true,
                );

                // 2. Stage payload data into physical blocks
                if prepared.blocks_needed > 0 && guard.durability_mode().is_range_based() {
                    payload_ranges.push((
                        (new_start_blk as usize).saturating_mul(BLOCK_SIZE),
                        prepared.blocks_needed.saturating_mul(BLOCK_SIZE),
                    ));
                }
                let mut written = 0;
                for b in 0..prepared.blocks_needed {
                    let cur_blk = new_start_blk + b as u64;
                    if let Some(slice) = Self::get_block_mut_from_map(&mut guard.mmap, cur_blk) {
                        let to_write = (prepared.payload.len() - written).min(BLOCK_SIZE);
                        slice[..to_write]
                            .copy_from_slice(&prepared.payload[written..written + to_write]);
                        if to_write < BLOCK_SIZE {
                            slice[to_write..].fill(0);
                        }
                        written += to_write;
                    }
                }

                // 3. Read old chunk entry
                let old_entry = crate::journal::sim_resolve_logical_chunk_entry(
                    &guard.mmap,
                    &inode,
                    prepared.chunk_idx,
                    &sim,
                );
                let old_bytes = if !old_entry.is_empty() {
                    old_entry.compressed_len() as i64
                } else {
                    0
                };
                compressed_size_delta += prepared.comp_len as i64 - old_bytes;

                // 4. Form new chunk entry and update pointer in sim & ops
                let new_entry = crate::inode::ChunkEntry::new(
                    new_start_blk as u32,
                    prepared.blocks_needed as u8,
                    prepared.flag,
                    prepared.comp_len,
                );
                crate::journal::sim_set_logical_chunk_entry(
                    &guard.mmap,
                    &mut inode,
                    prepared.chunk_idx,
                    new_entry,
                    &mut sim,
                    &mut journal_ops,
                )?;

                // 5. Free old blocks via AllocSim
                if !old_entry.is_empty() {
                    let old_start = old_entry.start_block() as u64;
                    let old_cnt = old_entry.block_count() as usize;
                    sim.free_contiguous(&guard.mmap, old_start, old_cnt)?;
                    crate::journal::push_data_bitmap_op(
                        &mut journal_ops,
                        old_start,
                        old_cnt as u32,
                        false,
                    );
                }
            }

            // Reclaim legacy non-seekable blocks when upgrading a non-empty file to chunked format.
            for blk in old_non_seekable_blocks {
                sim.free_contiguous(&guard.mmap, blk, 1)?;
                crate::journal::push_data_bitmap_op(&mut journal_ops, blk, 1, false);
            }

            // 6. Recalculate inode compressed size & metadata
            let total_physical_bytes =
                (inode.compressed_size as i64 + compressed_size_delta).max(0) as u64;

            inode.size = new_file_size;
            inode.compressed_size = total_physical_bytes;
            inode.modified_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();

            let post_image = Self::build_inode_post_image(guard, inode_id, &inode)?;
            journal_ops.push(crate::journal::MetadataOp::WriteInode {
                inode_id,
                inode_bytes: Box::new(post_image),
            });
            let journal_ops = if crate::journal::is_already_coalesced(&journal_ops) {
                journal_ops
            } else {
                crate::journal::coalesce_ops(journal_ops)
            };

            // 7. Flush payload blocks before metadata commit (Ordered Mode — Strict only)
            if guard.durability_mode() == DurabilityMode::Strict {
                let coalesced_payload =
                    DiskManagerInner::coalesce_ranges(&payload_ranges, guard.mmap.len());
                for (off, actual) in &coalesced_payload {
                    guard.mmap.flush_range(*off, *actual)?;
                }
            }

            // 8. Commit WAL transaction to journal ring
            Self::commit_journal_tx(guard, &journal_ops)?;

            // 9. Apply metadata changes in-place to mmap
            Self::apply_journal_ops(guard, &journal_ops)?;

            guard.free_inode_hint = sim.free_inode_hint;
            guard.free_block_hint = sim.free_block_hint;
            guard.inode_cache.insert(inode_id, inode);
            guard.invalidate_payload_caches(inode_id);

            if guard.durability_mode() == DurabilityMode::RangeAsync {
                let mut ranges = vec![
                    guard.data_bitmap_byte_range(),
                    guard.inode_byte_range(inode_id),
                ];
                ranges.extend(payload_ranges);
                for op in &journal_ops {
                    if let crate::journal::MetadataOp::WriteBlockSlice { block_id, .. } = op {
                        ranges.push(guard.block_byte_range(*block_id));
                    }
                }
                guard.sync_mutation_ranges(&ranges)?;
            }

            Self::maybe_checkpoint(guard)?;
            Ok(())
        } else {
            // Legacy in-place path (non-journaled)
            let mut compressed_size_delta: i64 = 0;
            let track_ranges = guard.durability_mode().is_range_based();
            let mut payload_ranges: Vec<(usize, usize)> = Vec::new();
            for prepared in prepared_chunks {
                // 1. Inspect existing chunk entry first: on non-journaled images, if the
                //    replacement fits inside the existing contiguous run (`blocks_needed <= old_cnt`),
                //    reuse `old_start` in-place to avoid bitmap search, allocation, and fragmentation.
                let old_entry =
                    Self::resolve_logical_chunk_entry(&guard.mmap, &inode, prepared.chunk_idx);
                let (old_bytes, old_start, old_cnt) = if !old_entry.is_empty() {
                    (
                        old_entry.compressed_len() as i64,
                        old_entry.start_block() as u64,
                        old_entry.block_count() as usize,
                    )
                } else {
                    (0, 0, 0)
                };
                compressed_size_delta += prepared.comp_len as i64 - old_bytes;

                let reuse_in_place = old_cnt > 0
                    && prepared.blocks_needed <= old_cnt
                    && guard.is_data_block(old_start);

                let new_start_blk = if reuse_in_place {
                    old_start
                } else {
                    Self::allocate_contiguous_blocks_internal(guard, prepared.blocks_needed)?
                };

                // 2. Write payload into target blocks
                let mut written = 0;
                for b in 0..prepared.blocks_needed {
                    let cur_blk = new_start_blk + b as u64;
                    if track_ranges {
                        payload_ranges.push(guard.block_byte_range(cur_blk));
                    }
                    if let Some(slice) = Self::get_block_mut_from_map(&mut guard.mmap, cur_blk) {
                        let to_write = (prepared.payload.len() - written).min(BLOCK_SIZE);
                        slice[..to_write]
                            .copy_from_slice(&prepared.payload[written..written + to_write]);
                        if to_write < BLOCK_SIZE {
                            slice[to_write..].fill(0);
                        }
                        written += to_write;
                    }
                }

                // 3. Form new chunk entry and update pointer if changed
                let new_entry = crate::inode::ChunkEntry::new(
                    new_start_blk as u32,
                    prepared.blocks_needed as u8,
                    prepared.flag,
                    prepared.comp_len,
                );
                if new_entry != old_entry {
                    Self::set_logical_chunk_entry(
                        guard,
                        &mut inode,
                        prepared.chunk_idx,
                        new_entry,
                        None,
                    )?;
                }

                // 4. Free any unused old blocks
                if reuse_in_place {
                    if prepared.blocks_needed < old_cnt {
                        let excess_start = old_start + prepared.blocks_needed as u64;
                        let excess_cnt = old_cnt - prepared.blocks_needed;
                        Self::free_contiguous_blocks_internal(guard, excess_start, excess_cnt)?;
                    }
                } else if old_cnt > 0 {
                    Self::free_contiguous_blocks_internal(guard, old_start, old_cnt)?;
                }
            }

            // Reclaim legacy non-seekable blocks when upgrading a non-empty file to chunked format.
            for blk in old_non_seekable_blocks {
                Self::free_contiguous_blocks_internal(guard, blk, 1)?;
            }

            // 6. Recalculate inode compressed size & metadata
            let total_physical_bytes =
                (inode.compressed_size as i64 + compressed_size_delta).max(0) as u64;

            inode.size = new_file_size;
            inode.compressed_size = total_physical_bytes;
            inode.modified_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();

            Self::write_inode_internal(guard, inode_id, &inode)?;
            if guard.durability_mode().is_range_based() {
                let mut ranges = vec![
                    guard.data_bitmap_byte_range(),
                    guard.inode_byte_range(inode_id),
                ];
                ranges.extend(payload_ranges);
                guard.sync_mutation_ranges(&ranges)?;
            }
            guard.inode_cache.insert(inode_id, inode);
            Ok(())
        }
    }

    pub(super) fn truncate_chunked_64k(
        guard: &mut DiskManagerInner,
        inode_id: u64,
        new_size: u64,
    ) -> Result<(), DiskManagerError> {
        let mut inode = Self::read_inode_internal(guard, inode_id)?;
        if inode.size == new_size {
            return Ok(());
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Case A: Expansion (sparse)
        if new_size > inode.size {
            inode.size = new_size;
            inode.modified_at = now;
            if guard.superblock.has_journal_layout() {
                let post_image = Self::build_inode_post_image(guard, inode_id, &inode)?;
                let ops = vec![crate::journal::MetadataOp::WriteInode {
                    inode_id,
                    inode_bytes: Box::new(post_image),
                }];
                Self::commit_journal_tx(guard, &ops)?;
                Self::apply_journal_ops(guard, &ops)?;
                guard.inode_cache.insert(inode_id, inode);
                Self::maybe_checkpoint(guard)?;
            } else {
                Self::write_inode_internal(guard, inode_id, &inode)?;
                if guard.durability_mode().is_range_based() {
                    let range = guard.inode_byte_range(inode_id);
                    guard.sync_mutation_ranges(&[range])?;
                }
                guard.inode_cache.insert(inode_id, inode);
            }
            return Ok(());
        }

        // Case B: Shrinking (new_size < inode.size)
        let old_chunks = (inode.size as usize).div_ceil(crate::inode::CHUNK_SIZE_64K);
        let new_chunks = (new_size as usize).div_ceil(crate::inode::CHUNK_SIZE_64K);

        if guard.superblock.has_journal_layout() {
            let mut sim = crate::journal::AllocSim::new_data_only(
                guard,
                guard.free_inode_hint,
                guard.free_block_hint,
            )?;
            let mut journal_ops: Vec<crate::journal::MetadataOp> = Vec::new();
            let mut payload_ranges: Vec<(usize, usize)> = Vec::new();
            let mut compressed_size_delta: i64 = 0;

            // 1. Free all chunks beyond new_chunks
            for c_idx in new_chunks..old_chunks {
                let entry = crate::journal::sim_resolve_logical_chunk_entry(
                    &guard.mmap,
                    &inode,
                    c_idx,
                    &sim,
                );
                if !entry.is_empty() {
                    compressed_size_delta -= entry.compressed_len() as i64;
                    let start = entry.start_block() as u64;
                    let count = entry.block_count() as usize;
                    sim.free_contiguous(&guard.mmap, start, count)?;
                    crate::journal::push_data_bitmap_op(
                        &mut journal_ops,
                        start,
                        count as u32,
                        false,
                    );
                    crate::journal::sim_set_logical_chunk_entry(
                        &guard.mmap,
                        &mut inode,
                        c_idx,
                        crate::inode::ChunkEntry::EMPTY,
                        &mut sim,
                        &mut journal_ops,
                    )?;
                }
            }

            // 1b. Prune and free any indirect pointer blocks beyond new_chunks
            let mut freed_indirect_blocks = Vec::new();
            {
                let mut ops_opt = Some(&mut journal_ops);
                Self::prune_unused_indirect_blocks(
                    guard,
                    &mut inode,
                    new_chunks,
                    &mut freed_indirect_blocks,
                    &mut ops_opt,
                );
            }
            for blk in freed_indirect_blocks {
                sim.free_contiguous(&guard.mmap, blk, 1)?;
                crate::journal::push_data_bitmap_op(&mut journal_ops, blk, 1, false);
            }

            // 2. If new_size == 0, clear all pointers
            if new_size == 0 {
                inode.blocks = [0; 12];
                inode.triple_indirect = 0;
                inode.compressed_size = 0;
                inode.size = 0;
                inode.modified_at = now;
            } else {
                let last_c_idx = new_chunks - 1;
                let last_chunk_start = (last_c_idx * crate::inode::CHUNK_SIZE_64K) as u64;
                let last_valid_len = (new_size - last_chunk_start) as usize;

                if last_valid_len < crate::inode::CHUNK_SIZE_64K {
                    let old_entry = crate::journal::sim_resolve_logical_chunk_entry(
                        &guard.mmap,
                        &inode,
                        last_c_idx,
                        &sim,
                    );
                    if !old_entry.is_empty() {
                        let old_data = Self::read_single_chunk_decompressed(
                            guard,
                            inode_id,
                            &inode,
                            last_c_idx,
                            last_valid_len,
                        )?;
                        let compress = old_entry.is_compressed() || guard.encryption_key.is_none();
                        let (payload, flag, comp_len, blocks_needed) = Self::prepare_chunk_payload(
                            &old_data,
                            last_valid_len,
                            1,
                            compress,
                            guard.encryption_key.as_ref(),
                            ChunkAadContext {
                                inode_id,
                                chunk_idx: last_c_idx as u64,
                                scheme: guard.superblock.encryption_version,
                            },
                        )?;
                        compressed_size_delta +=
                            comp_len as i64 - old_entry.compressed_len() as i64;

                        let new_start_blk = sim.alloc_contiguous(&guard.mmap, blocks_needed)?;
                        crate::journal::push_data_bitmap_op(
                            &mut journal_ops,
                            new_start_blk,
                            blocks_needed as u32,
                            true,
                        );

                        if blocks_needed > 0 && guard.durability_mode().is_range_based() {
                            payload_ranges.push((
                                (new_start_blk as usize).saturating_mul(BLOCK_SIZE),
                                blocks_needed.saturating_mul(BLOCK_SIZE),
                            ));
                        }
                        let mut written = 0;
                        for b in 0..blocks_needed {
                            let cur_blk = new_start_blk + b as u64;
                            if let Some(slice) =
                                Self::get_block_mut_from_map(&mut guard.mmap, cur_blk)
                            {
                                let to_write = (payload.len() - written).min(BLOCK_SIZE);
                                slice[..to_write]
                                    .copy_from_slice(&payload[written..written + to_write]);
                                if to_write < BLOCK_SIZE {
                                    slice[to_write..].fill(0);
                                }
                                written += to_write;
                            }
                        }

                        let new_entry = crate::inode::ChunkEntry::new(
                            new_start_blk as u32,
                            blocks_needed as u8,
                            flag,
                            comp_len,
                        );
                        crate::journal::sim_set_logical_chunk_entry(
                            &guard.mmap,
                            &mut inode,
                            last_c_idx,
                            new_entry,
                            &mut sim,
                            &mut journal_ops,
                        )?;

                        let old_start = old_entry.start_block() as u64;
                        let old_cnt = old_entry.block_count() as usize;
                        sim.free_contiguous(&guard.mmap, old_start, old_cnt)?;
                        crate::journal::push_data_bitmap_op(
                            &mut journal_ops,
                            old_start,
                            old_cnt as u32,
                            false,
                        );
                    }
                }

                // O(1) incremental compressed_size update instead of O(new_chunks) tree walk
                inode.compressed_size =
                    (inode.compressed_size as i64 + compressed_size_delta).max(0) as u64;
                inode.size = new_size;
                inode.modified_at = now;
            }

            let post_image = Self::build_inode_post_image(guard, inode_id, &inode)?;
            journal_ops.push(crate::journal::MetadataOp::WriteInode {
                inode_id,
                inode_bytes: Box::new(post_image),
            });

            // Ordered payload flush (Strict only)
            if guard.durability_mode() == DurabilityMode::Strict {
                let coalesced_payload =
                    DiskManagerInner::coalesce_ranges(&payload_ranges, guard.mmap.len());
                for (off, actual) in &coalesced_payload {
                    guard.mmap.flush_range(*off, *actual)?;
                }
            }

            let journal_ops = if crate::journal::is_already_coalesced(&journal_ops) {
                journal_ops
            } else {
                crate::journal::coalesce_ops(journal_ops)
            };
            Self::commit_journal_tx(guard, &journal_ops)?;
            Self::apply_journal_ops(guard, &journal_ops)?;

            guard.free_inode_hint = sim.free_inode_hint;
            guard.free_block_hint = sim.free_block_hint;
            guard.inode_cache.insert(inode_id, inode);
            guard.invalidate_payload_caches(inode_id);

            if guard.durability_mode() == DurabilityMode::RangeAsync {
                let mut ranges = vec![
                    guard.data_bitmap_byte_range(),
                    guard.inode_byte_range(inode_id),
                ];
                ranges.extend(payload_ranges);
                for op in &journal_ops {
                    if let crate::journal::MetadataOp::WriteBlockSlice { block_id, .. } = op {
                        ranges.push(guard.block_byte_range(*block_id));
                    }
                }
                guard.sync_mutation_ranges(&ranges)?;
            }

            Self::maybe_checkpoint(guard)?;
            return Ok(());
        }

        // Legacy non-journaled path
        let mut compressed_size_delta: i64 = 0;
        // 1. Free all chunks beyond new_chunks
        for c_idx in new_chunks..old_chunks {
            let entry = Self::resolve_logical_chunk_entry(&guard.mmap, &inode, c_idx);
            if !entry.is_empty() {
                compressed_size_delta -= entry.compressed_len() as i64;
                let start = entry.start_block() as u64;
                let count = entry.block_count() as usize;
                Self::free_contiguous_blocks_internal(guard, start, count)?;
                Self::set_logical_chunk_entry(
                    guard,
                    &mut inode,
                    c_idx,
                    crate::inode::ChunkEntry::EMPTY,
                    None,
                )?;
            }
        }

        // 1b. Prune and free any indirect pointer blocks beyond new_chunks
        let mut freed_indirect_blocks = Vec::new();
        {
            let mut ops_opt = None;
            Self::prune_unused_indirect_blocks(
                guard,
                &mut inode,
                new_chunks,
                &mut freed_indirect_blocks,
                &mut ops_opt,
            );
        }
        for blk in freed_indirect_blocks {
            Self::free_contiguous_blocks_internal(guard, blk, 1)?;
        }

        // 2. If new_size == 0, clear all pointers
        if new_size == 0 {
            inode.blocks = [0; 12];
            inode.triple_indirect = 0;
            inode.compressed_size = 0;
            inode.size = 0;
            inode.modified_at = now;
        } else {
            // Check if the last chunk needs partial truncation
            let last_c_idx = new_chunks - 1;
            let last_chunk_start = (last_c_idx * crate::inode::CHUNK_SIZE_64K) as u64;
            let last_valid_len = (new_size - last_chunk_start) as usize;

            if last_valid_len < crate::inode::CHUNK_SIZE_64K {
                let old_entry = Self::resolve_logical_chunk_entry(&guard.mmap, &inode, last_c_idx);
                if !old_entry.is_empty() {
                    let old_data = Self::read_single_chunk_decompressed(
                        guard,
                        inode_id,
                        &inode,
                        last_c_idx,
                        last_valid_len,
                    )?;
                    let compress = old_entry.is_compressed() || guard.encryption_key.is_none();
                    let (payload, flag, comp_len, blocks_needed) = Self::prepare_chunk_payload(
                        &old_data,
                        last_valid_len,
                        1,
                        compress,
                        guard.encryption_key.as_ref(),
                        ChunkAadContext {
                            inode_id,
                            chunk_idx: last_c_idx as u64,
                            scheme: guard.superblock.encryption_version,
                        },
                    )?;
                    compressed_size_delta += comp_len as i64 - old_entry.compressed_len() as i64;

                    let new_start_blk =
                        Self::allocate_contiguous_blocks_internal(guard, blocks_needed)?;
                    let mut written = 0;
                    for b in 0..blocks_needed {
                        let cur_blk = new_start_blk + b as u64;
                        if let Some(slice) = Self::get_block_mut_from_map(&mut guard.mmap, cur_blk)
                        {
                            let to_write = (payload.len() - written).min(BLOCK_SIZE);
                            slice[..to_write]
                                .copy_from_slice(&payload[written..written + to_write]);
                            if to_write < BLOCK_SIZE {
                                slice[to_write..].fill(0);
                            }
                            written += to_write;
                        }
                    }

                    let new_entry = crate::inode::ChunkEntry::new(
                        new_start_blk as u32,
                        blocks_needed as u8,
                        flag,
                        comp_len,
                    );
                    Self::set_logical_chunk_entry(guard, &mut inode, last_c_idx, new_entry, None)?;

                    let old_start = old_entry.start_block() as u64;
                    let old_cnt = old_entry.block_count() as usize;
                    Self::free_contiguous_blocks_internal(guard, old_start, old_cnt)?;
                }
            }

            // O(1) incremental compressed_size update instead of O(new_chunks) tree walk
            inode.compressed_size =
                (inode.compressed_size as i64 + compressed_size_delta).max(0) as u64;
            inode.size = new_size;
            inode.modified_at = now;
        }

        Self::write_inode_internal(guard, inode_id, &inode)?;
        if guard.durability_mode().is_range_based() {
            let range = guard.inode_byte_range(inode_id);
            guard.sync_mutation_ranges(&[range])?;
        }
        guard.inode_cache.insert(inode_id, inode);
        Ok(())
    }
}
