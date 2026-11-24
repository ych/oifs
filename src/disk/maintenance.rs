//! Online maintenance, migration, defragmentation, and integrity verification (`fsck`) for `DiskManager`:
//! - Inode format migration (`needs_migration`, `migration_in_progress`, `migrate`)
//! - Data-bitmap fragmentation analysis (`analyze_fragmentation`)
//! - Safe copy-on-write and WAL-atomic in-place defragmentation (`defragment`, `defragment_inplace_journaled_cow`)
//! - Full-image consistency checker (`verify_integrity`)

use super::*;

impl DiskManager {
    /// `true` when this image still stores inodes in the legacy format and would
    /// benefit from [`Self::migrate`].
    pub fn needs_migration(&self) -> bool {
        self.inner
            .read()
            .unwrap()
            .superblock
            .is_legacy_inode_format()
    }

    /// `true` when a previous migration was interrupted and is resumable.
    pub fn migration_in_progress(&self) -> bool {
        self.inner
            .read()
            .unwrap()
            .superblock
            .is_migration_in_progress()
    }

    /// Upgrade a legacy (v1) image to the current inode format, in place.
    ///
    /// # Crash safety
    ///
    /// Inodes are rewritten one at a time, so a crash can leave an image holding
    /// both encodings. That intermediate state is *recorded* rather than ambiguous:
    /// the superblock keeps advertising the legacy version while
    /// [`SuperBlock::migration_cursor`] marks how far the rewrite has progressed,
    /// and readers decode each slot according to its position (see
    /// [`SuperBlock::inode_record_version_for`]). A crash therefore leaves a
    /// readable, resumable image rather than a corrupt one.
    ///
    /// The journal is checkpointed first: a pending `WriteInode` op encoded in the
    /// outgoing format would otherwise be replayed into a rewritten slot and destroy
    /// the record.
    ///
    /// Idempotent — safe to call on an already-current image (returns zero work) and
    /// safe to call again after an interrupted migration.
    pub fn migrate(&self) -> Result<MigrationStats, DiskManagerError> {
        let mut guard = self.inner.write().unwrap();
        if guard.read_only {
            return Err(DiskManagerError::ReadOnly);
        }
        let from_version = guard.superblock.inode_record_version();
        if !guard.superblock.is_legacy_inode_format() {
            return Ok(MigrationStats {
                from_version,
                to_version: from_version,
                inodes_rewritten: 0,
                already_current: true,
            });
        }

        // Must precede any rewrite: see the doc comment.
        let sb = guard.superblock;
        Self::checkpoint_journal(&mut guard.backing, &sb)?;

        // Snapshot the allocation bitmap so we know which slots are in use.
        let inode_bitmap = guard
            .inode_bitmap_slice()
            .ok_or_else(|| DiskManagerError::Io(std::io::Error::other("inode bitmap not found")))?
            .to_vec();

        let mut cursor = guard.superblock.migration_cursor;
        let mut rewritten = 0u64;

        while cursor < sb.inode_count {
            let inode_id = cursor;
            let bit = inode_id as usize;
            let byte_idx = bit / 8;
            let allocated =
                byte_idx < inode_bitmap.len() && (inode_bitmap[byte_idx] & (1 << (bit % 8)) != 0);

            if allocated {
                // Decode with the *outgoing* format regardless of the cursor: a slot
                // at the cursor has not been rewritten yet.
                let (start, end) = Self::inode_slot_range(&guard, inode_id)?;
                let mut raw = [0u8; crate::inode_format::INODE_SLOT_SIZE];
                guard
                    .backing
                    .read_exact_at(start as u64, &mut raw[..end - start])?;
                let inode = crate::inode_format::decode_v1(&raw[..end - start])?;
                let slot = crate::inode_format::encode_v2(&inode);
                guard.backing.write_all_at(start as u64, &slot)?;
                // Flush the rewritten slot before advancing the durable cursor so a
                // crash never leaves `migration_cursor` pointing past an un-flushed v1 slot.
                guard.backing.flush_range(start, end - start)?;
                rewritten += 1;

                guard.superblock.migration_cursor = inode_id + 1;
                Self::persist_superblock(&mut guard)?;
                guard.backing.flush_range(0, BLOCK_SIZE)?;
            }

            cursor += 1;
        }

        // All slots rewritten: advertise the new format and clear the cursor.
        guard.superblock.migration_cursor = 0;
        guard.superblock.format_version = SuperBlock::FORMAT_VERSION_FIXED_INODE;
        Self::persist_superblock(&mut guard)?;
        guard.backing.flush()?;
        guard.inode_cache.clear();
        guard.clear_payload_caches();

        Ok(MigrationStats {
            from_version,
            to_version: SuperBlock::FORMAT_VERSION_FIXED_INODE,
            inodes_rewritten: rewritten,
            already_current: false,
        })
    }

    /// Write the in-memory superblock back into block 0.
    fn persist_superblock(guard: &mut DiskManagerInner) -> Result<(), DiskManagerError> {
        let bytes = bincode::serialize(&guard.superblock)?;
        if bytes.len() > BLOCK_SIZE {
            return Err(DiskManagerError::Serialization(Box::new(
                bincode::ErrorKind::SizeLimit,
            )));
        }
        guard.backing.write_all_at(0, &bytes)?;
        Ok(())
    }

    /// Analyzes disk fragmentation and returns statistics
    ///
    /// Scans the data block bitmap to calculate fragmentation metrics.
    ///
    /// # Returns
    /// `FragmentationStats` containing detailed fragmentation information
    /// Analyzes disk fragmentation and returns statistics
    ///
    /// Scans the data block bitmap to calculate fragmentation metrics.
    ///
    /// # Returns
    /// `FragmentationStats` containing detailed fragmentation information
    pub fn analyze_fragmentation(&self) -> Result<FragmentationStats, DiskManagerError> {
        let guard = self.inner.read().unwrap();
        Self::analyze_fragmentation_internal(&guard)
    }

    /// Fragmentation analysis driven by an `Arc<RwLock<DiskManagerInner>>`.
    pub(crate) fn analyze_fragmentation_on(
        inner: &Arc<RwLock<DiskManagerInner>>,
    ) -> Result<FragmentationStats, DiskManagerError> {
        let guard = inner.read().unwrap();
        Self::analyze_fragmentation_internal(&guard)
    }

    pub(crate) fn analyze_fragmentation_internal(
        guard: &DiskManagerInner,
    ) -> Result<FragmentationStats, DiskManagerError> {
        let sb = &guard.superblock;

        // The data bitmap may span several blocks.
        let bitmap_slice = guard
            .data_bitmap_slice()
            .ok_or_else(|| DiskManagerError::Io(std::io::Error::other("Bitmap not found")))?;

        // Zero-allocation BitmapRef analysis
        let bitmap = crate::bitmap::BitmapRef::new(bitmap_slice);
        let total_blocks = (sb.block_count - sb.data_block_start) as usize;

        let mut used_blocks = 0;
        let mut free_runs = 0;
        let mut current_run_len = 0;
        let mut largest_free_run = 0;
        let mut total_gap_size = 0;

        // Fast 64-bit word scanning
        let mut in_free_run = false;
        let num_words = total_blocks / 64;
        let rem_bits = total_blocks % 64;
        let (chunks, _) = bitmap_slice.as_chunks::<8>();

        for chunk in chunks.iter().take(num_words) {
            let word = u64::from_le_bytes(*chunk);
            if word == 0 {
                // All 64 blocks are free
                if !in_free_run {
                    free_runs += 1;
                    in_free_run = true;
                }
                current_run_len += 64;
            } else if word == u64::MAX {
                // All 64 blocks are used
                used_blocks += 64;
                if in_free_run {
                    total_gap_size += current_run_len;
                    largest_free_run = largest_free_run.max(current_run_len);
                    in_free_run = false;
                    current_run_len = 0;
                }
            } else {
                // Mixed bits: inspect bit by bit
                used_blocks += word.count_ones() as usize;
                for bit in 0..64 {
                    let is_free = (word & (1u64 << bit)) == 0;
                    if is_free {
                        if !in_free_run {
                            free_runs += 1;
                            in_free_run = true;
                        }
                        current_run_len += 1;
                    } else if in_free_run {
                        total_gap_size += current_run_len;
                        largest_free_run = largest_free_run.max(current_run_len);
                        in_free_run = false;
                        current_run_len = 0;
                    }
                }
            }
        }

        // Remainder bits if total_blocks % 64 != 0
        if rem_bits > 0 {
            let base_idx = num_words * 64;
            for i in base_idx..total_blocks {
                let is_free = !bitmap.get(i);
                if is_free {
                    if !in_free_run {
                        free_runs += 1;
                        in_free_run = true;
                    }
                    current_run_len += 1;
                } else {
                    used_blocks += 1;
                    if in_free_run {
                        total_gap_size += current_run_len;
                        largest_free_run = largest_free_run.max(current_run_len);
                        in_free_run = false;
                        current_run_len = 0;
                    }
                }
            }
        }

        // Handle final free run if exists
        if in_free_run {
            total_gap_size += current_run_len;
            largest_free_run = largest_free_run.max(current_run_len);
        }

        let free_blocks = total_blocks - used_blocks;
        let avg_gap_size = if free_runs > 0 {
            total_gap_size as f64 / free_runs as f64
        } else {
            0.0
        };

        // Calculate fragmentation ratio
        // Ideal: all free space in one contiguous run (free_runs = 1 or 0)
        // Worst: each free block is separate (free_runs = free_blocks)
        let fragmentation_ratio = if free_blocks > 0 && free_runs > 1 {
            (free_runs - 1) as f64 / (free_blocks - 1).max(1) as f64
        } else {
            0.0
        };

        Ok(FragmentationStats {
            total_blocks,
            used_blocks,
            free_blocks,
            free_runs,
            largest_free_run,
            avg_gap_size,
            fragmentation_ratio,
        })
    }

    /// Reloads the underlying image backing and locks after an external or safe-mode file replacement.
    pub(crate) fn reload_from_disk(&self, source_path: &str) -> Result<(), DiskManagerError> {
        let mut guard = self.inner.write().unwrap();
        if guard.read_only {
            return Err(DiskManagerError::ReadOnly);
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(source_path)?;

        Self::try_lock_whole_file(&file)?;

        let storage_mode = guard.backing.mode();
        let mut backing = crate::storage::ImageBacking::open(&file, storage_mode)?;
        let mut sb_buf = [0u8; BLOCK_SIZE];
        backing.read_exact_at(0, &mut sb_buf)?;
        let superblock: SuperBlock = bincode::deserialize(&sb_buf)?;
        superblock
            .validate(backing.len())
            .map_err(DiskManagerError::CorruptedSuperblock)?;
        backing.configure_regions(&superblock)?;

        guard.file = file;
        guard.backing_path =
            std::fs::canonicalize(source_path).unwrap_or_else(|_| PathBuf::from(source_path));
        guard.backing = backing;
        guard.superblock = superblock;
        guard.free_block_hint = superblock.data_block_start;
        guard.free_inode_hint = 0;
        guard.inode_cache = BoundedInodeCache::with_capacity(INODE_CACHE_CAPACITY);
        guard.dir_cache.clear();
        guard.clear_payload_caches();
        Ok(())
    }

    /// Collects all physical block IDs for an inode split into:
    /// 1. `payload_blocks`: data/directory blocks in logical order
    /// 2. `indirect_blocks`: pointer table blocks (SIB, DIB, TIB)
    fn collect_inode_block_layout(
        backing: &crate::storage::ImageBacking,
        inode: &Inode,
    ) -> (Vec<u64>, Vec<u64>) {
        let mut payload_blocks = Vec::new();
        let mut indirect_blocks = Vec::new();

        // A seekable 64K file does not store raw block ids in its
        // pointer slots but packed `ChunkEntry` values, whose payload is a
        // *run* of contiguous blocks. Treating the packed value as a block id
        // would relocate the wrong blocks (or none at all) and destroy the
        // file.
        let is_seekable = inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K != 0;

        // Expands a pointer slot into the blocks it addresses.
        let push_leaf = |raw: u64, payload_blocks: &mut Vec<u64>| {
            if raw == 0 {
                return;
            }
            if is_seekable {
                let entry = crate::inode::ChunkEntry(raw);
                if entry.is_empty() {
                    return;
                }
                let start = entry.start_block() as u64;
                let count = entry.block_count() as u64;
                for i in 0..count {
                    payload_blocks.push(start + i);
                }
            } else {
                payload_blocks.push(raw);
            }
        };

        // 1. Direct blocks (0..10)
        for i in 0..10 {
            push_leaf(inode.blocks[i], &mut payload_blocks);
        }

        // 2. Single Indirect block (10)
        let sib_id = inode.blocks[10];
        if sib_id != 0 {
            indirect_blocks.push(sib_id);
            let _ = backing.with_block(sib_id, |slice| {
                for chunk in slice.as_chunks::<8>().0 {
                    let blk = u64::from_le_bytes(*chunk);
                    push_leaf(blk, &mut payload_blocks);
                }
            });
        }

        // 3. Double Indirect block (11)
        let dib_id = inode.blocks[11];
        if dib_id != 0 {
            indirect_blocks.push(dib_id);
            let sibs: Vec<u64> = backing
                .with_block(dib_id, |slice| {
                    slice
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .map(|c| u64::from_le_bytes(*c))
                        .filter(|&sib| sib != 0)
                        .collect()
                })
                .unwrap_or_default();
            for sib in sibs {
                indirect_blocks.push(sib);
                let _ = backing.with_block(sib, |s_slice| {
                    for d_chunk in s_slice.as_chunks::<8>().0 {
                        let blk = u64::from_le_bytes(*d_chunk);
                        push_leaf(blk, &mut payload_blocks);
                    }
                });
            }
        }

        // 4. Triple Indirect block
        let tib_id = inode.triple_indirect;
        if tib_id != 0 {
            indirect_blocks.push(tib_id);
            let dibs: Vec<u64> = backing
                .with_block(tib_id, |slice| {
                    slice
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .map(|c| u64::from_le_bytes(*c))
                        .filter(|&dib| dib != 0)
                        .collect()
                })
                .unwrap_or_default();
            for dib in dibs {
                indirect_blocks.push(dib);
                let sibs: Vec<u64> = backing
                    .with_block(dib, |d_slice| {
                        d_slice
                            .as_chunks::<8>()
                            .0
                            .iter()
                            .map(|c| u64::from_le_bytes(*c))
                            .filter(|&sib| sib != 0)
                            .collect()
                    })
                    .unwrap_or_default();
                for sib in sibs {
                    indirect_blocks.push(sib);
                    let _ = backing.with_block(sib, |s_slice| {
                        for p_chunk in s_slice.as_chunks::<8>().0 {
                            let blk = u64::from_le_bytes(*p_chunk);
                            push_leaf(blk, &mut payload_blocks);
                        }
                    });
                }
            }
        }

        (payload_blocks, indirect_blocks)
    }

    /// Defragments the filesystem by reorganizing files contiguously
    ///
    /// # Arguments
    /// * `source_path` - Path to the source image file
    /// * `mode` - Defragmentation mode (Safe or InPlace)
    /// * `output_path` - Optional output path for safe mode (defaults to source_path.defrag.tmp)
    ///
    /// # Returns
    /// Statistics about the defragmentation operation
    pub fn defragment(
        &self,
        source_path: &str,
        mode: DefragMode,
        output_path: Option<&str>,
    ) -> Result<DefragStats, DiskManagerError> {
        if self.is_read_only() {
            return Err(DiskManagerError::ReadOnly);
        }
        let backing = self.backing_path();
        let canonical_source = std::fs::canonicalize(source_path).map_err(DiskManagerError::Io)?;
        if canonical_source != backing {
            return Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "Defragment source path ({:?}) does not match opened disk image ({:?})",
                    canonical_source, backing
                ),
            )));
        }

        if let Some(out) = output_path {
            let out_path = Path::new(out);
            if out.is_empty()
                || out_path
                    .components()
                    .any(|c| c == std::path::Component::ParentDir)
            {
                return Err(DiskManagerError::Io(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "Invalid defragment output path",
                )));
            }
            let out_parent = match out_path.parent() {
                Some(p) if !p.as_os_str().is_empty() => p,
                _ => Path::new("."),
            };
            let canonical_out_parent =
                std::fs::canonicalize(out_parent).map_err(DiskManagerError::Io)?;
            let backing_parent = match backing.parent() {
                Some(p) if !p.as_os_str().is_empty() => p,
                _ => Path::new("."),
            };
            let canonical_backing_parent = std::fs::canonicalize(backing_parent)
                .unwrap_or_else(|_| backing_parent.to_path_buf());
            if canonical_out_parent != canonical_backing_parent {
                return Err(DiskManagerError::Io(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "Defragment output path must reside in the same directory as the disk image",
                )));
            }
            if let Ok(canon_out) = std::fs::canonicalize(out_path)
                && canon_out == backing
            {
                return Err(DiskManagerError::Io(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "Defragment output path cannot overwrite the source image directly",
                )));
            }
        }

        match mode {
            DefragMode::Safe => self.defragment_safe(source_path, output_path),
            DefragMode::InPlace => self.defragment_inplace(),
        }
    }

    /// Safe defragmentation: creates new image with defragmented layout,
    /// stream-rewrites files to prevent memory exhaustion (OOM), supports encrypted
    /// images, and live-reloads the active handle after atomic swap.
    fn defragment_safe(
        &self,
        source_path: &str,
        output_path: Option<&str>,
    ) -> Result<DefragStats, DiskManagerError> {
        // Get fragmentation before
        let stats_before = self.analyze_fragmentation()?;

        // Determine temp path
        let temp_path = if let Some(out) = output_path {
            out.to_string()
        } else {
            format!("{}.defrag.tmp", source_path)
        };

        // Step 1: Copy the entire image file first (safer than creating new)
        std::fs::copy(source_path, &temp_path)?;

        // Step 2: Open the copy with preserved encryption key and storage mode
        let (enc_key, storage_mode) = {
            let g = self.inner.read().unwrap();
            (g.encryption_key.clone(), g.backing.mode())
        };
        let temp_dm = DiskManager::open_with_key_and_storage_mode(
            &temp_path,
            0,
            enc_key.clone(),
            storage_mode,
        )?;

        // Collect all active, allocated inodes and lightweight metadata (no file data in memory!)
        let sb = temp_dm.superblock();
        let mut file_meta_list = Vec::new();
        let mut directory_blocks = Vec::new();

        let mut allocated_inodes = Vec::new();
        {
            let guard = temp_dm.inner.read().unwrap();
            if let Some(bitmap_slice) = guard.inode_bitmap_slice() {
                let bitmap = crate::bitmap::BitmapRef::new(bitmap_slice);
                bitmap.for_each_set_bit(sb.inode_count as usize, |idx| {
                    allocated_inodes.push(idx as u64);
                });
            }
        }

        for inode_id in allocated_inodes {
            if let Ok(inode) = temp_dm.read_inode(inode_id) {
                if inode.mode == crate::inode::FileType::Directory {
                    let guard = temp_dm.inner.read().unwrap();
                    let dir_blks = Self::collect_inode_blocks(&guard.backing, &inode);
                    directory_blocks.extend(dir_blks);
                } else if inode.mode == crate::inode::FileType::File
                    && (inode.size > 0
                        || inode.compressed_size > 0
                        || inode.blocks.iter().any(|&b| b != 0)
                        || inode.triple_indirect != 0)
                {
                    // A seekable 64K file has `compressed_size == 0`
                    // but its payload *is* chunk-compressed. Copying it as a
                    // plain file would leave the seekable flag in place and
                    // make the reader interpret raw bytes as chunk entries.
                    let comp_mode = if inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K != 0 {
                        CompressionMode::Seekable {
                            chunk_size: crate::inode::CHUNK_SIZE_64K as u32,
                            level: 0,
                        }
                    } else if inode.compressed_size > 0 {
                        CompressionMode::Always
                    } else {
                        CompressionMode::Never
                    };
                    let filter_cfg = crate::filters::FilterConfig {
                        typesize: inode.filter_typesize,
                        delta: inode.filter_delta,
                        shuffle: inode.filter_shuffle,
                        bitshuffle: inode.filter_bitshuffle,
                    };
                    file_meta_list.push((inode_id, comp_mode, filter_cfg));

                    // Reset inode on disk so write_data allocates contiguous blocks
                    let mut cleared_inode = inode;
                    cleared_inode.size = 0;
                    cleared_inode.compressed_size = 0;
                    cleared_inode.blocks = [0; 12];
                    cleared_inode.triple_indirect = 0;
                    temp_dm.write_inode(inode_id, &cleared_inode)?;
                }
            }
        }

        // Step 3: Clear data bitmap and preserve directory blocks
        {
            let mut guard = temp_dm.inner.write().unwrap();
            let data_start = guard.superblock.data_block_start;
            if let Some(mut bitmap_slice) = guard.data_bitmap_slice_mut() {
                bitmap_slice.fill(0);
                let mut bitmap = crate::bitmap::Bitmap::new(&mut bitmap_slice);
                for &dir_blk in &directory_blocks {
                    if dir_blk >= data_start {
                        let bit_idx = (dir_blk - data_start) as usize;
                        bitmap.set(bit_idx);
                    }
                }
            }
        }

        // Step 4: Stream files one-by-one from self into temp_dm (bounded memory, no OOM)
        let mut files_processed = 0;
        let mut bytes_moved = 0u64;

        for (inode_id, comp_mode, filter_cfg) in file_meta_list {
            let data = self.read_data(inode_id)?;
            bytes_moved += data.len() as u64;
            temp_dm.write_data_with_filters(inode_id, 0, &data, comp_mode, filter_cfg)?;
            files_processed += 1;
        }

        // Step 5: Flush all changes
        temp_dm.flush()?;
        drop(temp_dm);

        // Step 6: Safe replacement using 3-step rename
        let backup_path = format!("{}.old", source_path);

        // 6a: Rename original to backup
        std::fs::rename(source_path, &backup_path)?;

        // 6b: Rename new defragged to original
        match std::fs::rename(&temp_path, source_path) {
            Ok(_) => {
                // Verify with preserved encryption key and storage mode
                match DiskManager::open_with_key_and_storage_mode(
                    source_path,
                    0,
                    enc_key,
                    storage_mode,
                ) {
                    Ok(final_dm) => {
                        let stats_after = final_dm.analyze_fragmentation()?;
                        let fsck = final_dm.verify_integrity()?;
                        if !fsck.is_clean {
                            let _ = std::fs::rename(&backup_path, source_path);
                            return Err(DiskManagerError::Io(std::io::Error::other(format!(
                                "Safe defragmentation integrity check failed: {:?}",
                                fsck
                            ))));
                        }

                        // Everything OK, drop final verification handle and delete backup
                        drop(final_dm);
                        let _ = std::fs::remove_file(&backup_path);

                        // Live reload the active manager handle to synchronize with the new file
                        self.reload_from_disk(source_path)?;

                        Ok(DefragStats {
                            files_processed,
                            bytes_moved,
                            blocks_freed: stats_before
                                .free_runs
                                .saturating_sub(stats_after.free_runs),
                            frag_before: stats_before.fragmentation_ratio,
                            frag_after: stats_after.fragmentation_ratio,
                        })
                    }
                    Err(e) => {
                        // Failed to open new file! Restore from backup
                        let _ = std::fs::rename(&backup_path, source_path);
                        Err(e)
                    }
                }
            }
            Err(e) => {
                // Failed to rename new file! Restore original
                let _ = std::fs::rename(&backup_path, source_path);
                Err(DiskManagerError::Io(e))
            }
        }
    }

    /// In-place defragmentation: directly compacts and reorganizes physical data blocks
    /// within the existing image without allocating duplicate temporary files.
    fn defragment_inplace(&self) -> Result<DefragStats, DiskManagerError> {
        Self::defragment_inplace_on(&self.inner)
    }

    /// In-place defragmentation driven by an `Arc<RwLock<DiskManagerInner>>`.
    ///
    /// Exposed separately from [`Self::defragment_inplace`] so the background
    /// auto-defrag coordinator can run a pass without owning a `DiskManager`.
    pub(crate) fn defragment_inplace_on(
        inner: &Arc<RwLock<DiskManagerInner>>,
    ) -> Result<DefragStats, DiskManagerError> {
        let mut guard = inner.write().unwrap();
        if guard.read_only {
            return Err(DiskManagerError::ReadOnly);
        }
        let stats_before = Self::analyze_fragmentation_internal(&guard)?;

        // Exact zero-fragmentation short-circuit when all allocated blocks occupy
        // `[0..used_blocks)` and all free blocks occupy `[used_blocks..total_blocks)`.
        let already_compacted = stats_before.used_blocks == 0
            || stats_before.used_blocks == stats_before.total_blocks
            || (stats_before.free_runs == 1 && {
                guard.data_bitmap_slice().is_some_and(|s| {
                    !crate::bitmap::BitmapRef::new(s).get(stats_before.total_blocks - 1)
                })
            });
        if already_compacted {
            return Ok(DefragStats {
                files_processed: 0,
                bytes_moved: 0,
                blocks_freed: 0,
                frag_before: stats_before.fragmentation_ratio,
                frag_after: stats_before.fragmentation_ratio,
            });
        }

        let sb = guard.superblock;
        let is_journaled = sb.has_journal_layout();
        let bs = sb.block_size as u64;

        // Phase 0: If journaled, pre-defrag quiesce:
        // 1. Drain pending async ranges (RangeAsync durability mode).
        // 2. Flush dirty pages to disk so in-place data matches previous WAL commits.
        // 3. Checkpoint WAL to head so the WAL ring is clean (used() == 0).
        if is_journaled {
            guard.drain_pending_async_ranges_internal();
            guard.backing.flush()?;
            {
                let mut region = guard.backing.journal_region_mut(&sb)?;
                region.mark_header_only();
                let mut ring = crate::journal::JournalRing::open(&mut region, bs)?;
                ring.checkpoint();
                ring.persist();
            }
            Self::sync_journal_region(&guard)?;
        }

        // 1. Collect and decode all active allocated inodes in a single pass without
        //    thrashing `inode_cache`.
        let mut allocated_ids = Vec::new();
        if let Some(bitmap_slice) = guard.inode_bitmap_slice() {
            let bitmap = crate::bitmap::BitmapRef::new(bitmap_slice);
            bitmap.for_each_set_bit(sb.inode_count as usize, |idx| {
                allocated_ids.push(idx as u64);
            });
        }

        let mut dir_inodes: Vec<(u64, Inode)> = Vec::new();
        let mut file_inodes: Vec<(u64, Inode)> = Vec::new();
        let mut other_inodes: Vec<(u64, Inode)> = Vec::new();
        for inode_id in allocated_ids {
            let inode = Self::read_inode_uncached(&guard, inode_id)?;
            if inode.mode == crate::inode::FileType::Directory {
                dir_inodes.push((inode_id, inode));
            } else if inode.mode == crate::inode::FileType::File && inode.size > 0 {
                file_inodes.push((inode_id, inode));
            } else {
                other_inodes.push((inode_id, inode));
            }
        }
        let files_processed = file_inodes.len();

        let bytes_moved = if is_journaled {
            Self::defragment_inplace_journaled_cow(&mut guard, dir_inodes, file_inodes)?
        } else {
            Self::defragment_inplace_legacy(
                &mut guard,
                &stats_before,
                dir_inodes,
                file_inodes,
                other_inodes,
            )?
        };

        // Fragmentation after
        let stats_after = Self::analyze_fragmentation_internal(&guard)?;
        drop(guard);

        // Verify filesystem consistency
        let fsck = Self::verify_integrity_on(inner)?;
        if !fsck.is_clean {
            return Err(DiskManagerError::Io(std::io::Error::other(format!(
                "In-place defragmentation integrity check failed: {:?}",
                fsck
            ))));
        }

        Ok(DefragStats {
            files_processed,
            bytes_moved,
            blocks_freed: stats_before.free_runs.saturating_sub(stats_after.free_runs),
            frag_before: stats_before.fragmentation_ratio,
            frag_after: stats_after.fragmentation_ratio,
        })
    }

    /// Legacy non-journaled in-place defragmentation using cycle-aware permutation.
    fn defragment_inplace_legacy(
        guard: &mut DiskManagerInner,
        stats_before: &FragmentationStats,
        mut dir_inodes: Vec<(u64, Inode)>,
        mut file_inodes: Vec<(u64, Inode)>,
        mut other_inodes: Vec<(u64, Inode)>,
    ) -> Result<u64, DiskManagerError> {
        // 2. Compute contiguous target locations for all blocks
        let mut next_target = guard.superblock.data_block_start;
        let mut old_to_new: FxHashMap<u64, u64> =
            FxHashMap::with_capacity_and_hasher(stats_before.used_blocks, Default::default());
        let mut all_indirect_blocks: FxHashSet<u64> = FxHashSet::default();

        // Place directory blocks first
        for (_, inode) in &dir_inodes {
            let (payloads, indirects) = Self::collect_inode_block_layout(&guard.backing, inode);
            for blk in payloads {
                old_to_new.insert(blk, next_target);
                next_target += 1;
            }
            for ind in indirects {
                all_indirect_blocks.insert(ind);
                old_to_new.insert(ind, next_target);
                next_target += 1;
            }
        }

        // Place regular file blocks next (including seekable 64K ChunkEntry runs)
        let mut chunk_entries: Vec<(u64, crate::inode::ChunkEntry)> = Vec::new();
        for (_, inode) in &file_inodes {
            let (payloads, indirects) = Self::collect_inode_block_layout(&guard.backing, inode);
            for blk in payloads {
                old_to_new.insert(blk, next_target);
                next_target += 1;
            }
            for ind in indirects {
                all_indirect_blocks.insert(ind);
                old_to_new.insert(ind, next_target);
                next_target += 1;
            }
            chunk_entries.extend(Self::collect_chunk_entries(&guard.backing, inode));
        }

        let mut old_entry_to_new: FxHashMap<u64, u64> = FxHashMap::default();
        for (packed, entry) in &chunk_entries {
            let Some(&new_start) = old_to_new.get(&(entry.start_block() as u64)) else {
                continue;
            };
            old_entry_to_new.insert(
                *packed,
                crate::inode::ChunkEntry::new(
                    new_start as u32,
                    entry.block_count(),
                    entry.flags(),
                    entry.compressed_len(),
                )
                .0,
            );
        }

        // 3. Filter blocks that need to move
        let mut pending: FxHashMap<u64, u64> = old_to_new
            .iter()
            .filter_map(|(&s, &d)| if s != d { Some((s, d)) } else { None })
            .collect();

        let num_moves = pending.len();
        let bytes_moved = num_moves as u64 * BLOCK_SIZE as u64;

        if pending.is_empty() {
            return Ok(0);
        }

        // 4. Cycle-aware block permutation
        let mut occupied: FxHashSet<u64> = pending.keys().copied().collect();
        let mut incoming: FxHashMap<u64, u64> = pending.iter().map(|(&s, &d)| (d, s)).collect();
        let mut free_targets: Vec<u64> = pending
            .iter()
            .filter_map(|(&s, &d)| {
                if !occupied.contains(&d) {
                    Some(s)
                } else {
                    None
                }
            })
            .collect();

        while !pending.is_empty() {
            if let Some(src) = free_targets.pop() {
                if let Some(dst) = pending.remove(&src) {
                    guard.backing.copy_block(src, dst)?;
                    occupied.remove(&src);
                    incoming.remove(&dst);

                    if let Some(&prev_src) = incoming.get(&src) {
                        free_targets.push(prev_src);
                    }
                }
            } else {
                let (&leader_src, _) = pending.iter().next().unwrap();
                let mut scratch = [0u8; BLOCK_SIZE];
                let leader_off = leader_src * BLOCK_SIZE as u64;
                guard.backing.read_exact_at(leader_off, &mut scratch)?;
                occupied.remove(&leader_src);

                let mut curr_dest = leader_src;
                while let Some(&curr_src) = incoming.get(&curr_dest) {
                    if curr_src == leader_src {
                        break;
                    }
                    guard.backing.copy_block(curr_src, curr_dest)?;

                    pending.remove(&curr_src);
                    occupied.remove(&curr_src);
                    incoming.remove(&curr_dest);

                    curr_dest = curr_src;
                }

                let final_off = curr_dest * BLOCK_SIZE as u64;
                guard.backing.write_all_at(final_off, &scratch)?;
                pending.remove(&leader_src);
                incoming.remove(&curr_dest);
            }
        }

        // 5. Update inode block pointers
        let lookup_new = |value: u64| -> Option<u64> {
            old_to_new
                .get(&value)
                .copied()
                .or_else(|| old_entry_to_new.get(&value).copied())
        };
        for (inode_id, inode) in dir_inodes
            .iter_mut()
            .chain(file_inodes.iter_mut())
            .chain(other_inodes.iter_mut())
        {
            let mut modified = false;
            for i in 0..10 {
                if inode.blocks[i] != 0
                    && let Some(new_blk) = lookup_new(inode.blocks[i])
                {
                    inode.blocks[i] = new_blk;
                    modified = true;
                }
            }
            if inode.blocks[10] != 0
                && let Some(&new_sib) = old_to_new.get(&inode.blocks[10])
            {
                inode.blocks[10] = new_sib;
                modified = true;
            }
            if inode.blocks[11] != 0
                && let Some(&new_dib) = old_to_new.get(&inode.blocks[11])
            {
                inode.blocks[11] = new_dib;
                modified = true;
            }
            if inode.triple_indirect != 0
                && let Some(&new_tib) = old_to_new.get(&inode.triple_indirect)
            {
                inode.triple_indirect = new_tib;
                modified = true;
            }
            if modified {
                Self::write_inode_internal(guard, *inode_id, inode)?;
            }
        }

        // 6. Update indirect block pointers
        for &old_ind_blk in &all_indirect_blocks {
            let new_ind_blk = *old_to_new.get(&old_ind_blk).unwrap_or(&old_ind_blk);
            let _ = guard.backing.with_block_mut(new_ind_blk, |slice| {
                for chunk in slice.as_chunks_mut::<8>().0 {
                    let ptr = u64::from_le_bytes(*chunk);
                    if ptr != 0
                        && let Some(new_ptr) = lookup_new(ptr)
                    {
                        *chunk = new_ptr.to_le_bytes();
                    }
                }
            });
        }

        // 7. Update data bitmap via word-aligned `set_range`
        let total_used = (next_target - guard.superblock.data_block_start) as usize;
        if let Some(mut bitmap_slice) = guard.data_bitmap_slice_mut() {
            bitmap_slice.fill(0);
            let mut bitmap = crate::bitmap::Bitmap::new(&mut bitmap_slice);
            bitmap.set_range(0, total_used);
        }

        // 8. Update hints & remap cached directory block IDs in place
        guard.free_block_hint = next_target;
        Self::remap_dir_cache_and_clear_payloads(guard, &old_to_new);
        match guard.durability_mode() {
            DurabilityMode::Lazy => {}
            DurabilityMode::Strict => {
                guard.backing.flush()?;
            }
            DurabilityMode::RangeAsync | DurabilityMode::LegacyWholeMmapAsync => {
                let _ = guard.backing.flush_async();
            }
        }
        Ok(bytes_moved)
    }

    /// WAL-atomic round-based copy-on-write in-place defragmentation.
    ///
    /// Every round copies blocks exclusively into currently free blocks, flushes
    /// those destination blocks, commits a single bounded WAL transaction (never
    /// overflowing the ring), applies the metadata updates via `apply_journal_ops`,
    /// flushes metadata, and checkpoints the WAL before the next round reuses the
    /// freed source blocks.
    fn defragment_inplace_journaled_cow(
        guard: &mut DiskManagerInner,
        dir_inodes: Vec<(u64, Inode)>,
        file_inodes: Vec<(u64, Inode)>,
    ) -> Result<u64, DiskManagerError> {
        #[derive(Clone, Copy)]
        enum UnitOwner {
            InodeDirect {
                inode_idx: usize,
                slot: usize,
            },
            InodeSib {
                inode_idx: usize,
            },
            InodeDib {
                inode_idx: usize,
            },
            InodeTib {
                inode_idx: usize,
            },
            IndirectSlot {
                parent_ind_idx: usize,
                slot_idx: usize,
            },
        }

        struct RelocUnit {
            curr_start: u64,
            block_count: u64,
            target_start: u64,
            owner: UnitOwner,
            chunk_entry: Option<crate::inode::ChunkEntry>,
            self_ind_idx: Option<usize>,
        }

        let sb = guard.superblock;
        let bs = sb.block_size as u64;
        let db_start = sb.data_block_start;
        let total_data = sb.block_count.saturating_sub(db_start) as usize;

        let mut inodes: Vec<(u64, Inode)> = dir_inodes;
        inodes.extend(file_inodes);

        let mut units: Vec<RelocUnit> = Vec::new();
        let mut indirect_blocks: Vec<u64> = Vec::new();
        let mut old_to_new: FxHashMap<u64, u64> = FxHashMap::default();
        let mut next_target = db_start;

        let read_slots = |backing: &crate::storage::ImageBacking, blk: u64| -> Vec<u64> {
            backing
                .with_block(blk, |s| {
                    s.as_chunks::<8>()
                        .0
                        .iter()
                        .map(|c| u64::from_le_bytes(*c))
                        .collect()
                })
                .unwrap_or_default()
        };

        for (inode_idx, (_, inode)) in inodes.iter().enumerate() {
            let is_seekable = inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K != 0;
            let mut deferred_inds: Vec<(u64, UnitOwner, usize)> = Vec::new();

            let mut push_leaf =
                |raw: u64, owner: UnitOwner, next_t: &mut u64, units: &mut Vec<RelocUnit>| {
                    if raw == 0 {
                        return;
                    }
                    if is_seekable {
                        let entry = crate::inode::ChunkEntry(raw);
                        if entry.is_empty() {
                            return;
                        }
                        let start = entry.start_block() as u64;
                        let count = entry.block_count() as u64;
                        let target = *next_t;
                        *next_t += count;
                        for k in 0..count {
                            old_to_new.insert(start + k, target + k);
                        }
                        units.push(RelocUnit {
                            curr_start: start,
                            block_count: count,
                            target_start: target,
                            owner,
                            chunk_entry: Some(entry),
                            self_ind_idx: None,
                        });
                    } else {
                        let target = *next_t;
                        *next_t += 1;
                        old_to_new.insert(raw, target);
                        units.push(RelocUnit {
                            curr_start: raw,
                            block_count: 1,
                            target_start: target,
                            owner,
                            chunk_entry: None,
                            self_ind_idx: None,
                        });
                    }
                };

            for slot in 0..10 {
                push_leaf(
                    inode.blocks[slot],
                    UnitOwner::InodeDirect { inode_idx, slot },
                    &mut next_target,
                    &mut units,
                );
            }

            if inode.blocks[10] != 0 {
                let sib = inode.blocks[10];
                let sib_idx = indirect_blocks.len();
                indirect_blocks.push(sib);
                deferred_inds.push((sib, UnitOwner::InodeSib { inode_idx }, sib_idx));
                for (slot_idx, raw) in read_slots(&guard.backing, sib).into_iter().enumerate() {
                    push_leaf(
                        raw,
                        UnitOwner::IndirectSlot {
                            parent_ind_idx: sib_idx,
                            slot_idx,
                        },
                        &mut next_target,
                        &mut units,
                    );
                }
            }

            if inode.blocks[11] != 0 {
                let dib = inode.blocks[11];
                let dib_idx = indirect_blocks.len();
                indirect_blocks.push(dib);
                deferred_inds.push((dib, UnitOwner::InodeDib { inode_idx }, dib_idx));
                for (dib_slot, sib) in read_slots(&guard.backing, dib).into_iter().enumerate() {
                    if sib != 0 {
                        let sib_idx = indirect_blocks.len();
                        indirect_blocks.push(sib);
                        deferred_inds.push((
                            sib,
                            UnitOwner::IndirectSlot {
                                parent_ind_idx: dib_idx,
                                slot_idx: dib_slot,
                            },
                            sib_idx,
                        ));
                        for (slot_idx, raw) in
                            read_slots(&guard.backing, sib).into_iter().enumerate()
                        {
                            push_leaf(
                                raw,
                                UnitOwner::IndirectSlot {
                                    parent_ind_idx: sib_idx,
                                    slot_idx,
                                },
                                &mut next_target,
                                &mut units,
                            );
                        }
                    }
                }
            }

            if inode.triple_indirect != 0 {
                let tib = inode.triple_indirect;
                let tib_idx = indirect_blocks.len();
                indirect_blocks.push(tib);
                deferred_inds.push((tib, UnitOwner::InodeTib { inode_idx }, tib_idx));
                for (tib_slot, dib) in read_slots(&guard.backing, tib).into_iter().enumerate() {
                    if dib != 0 {
                        let dib_idx = indirect_blocks.len();
                        indirect_blocks.push(dib);
                        deferred_inds.push((
                            dib,
                            UnitOwner::IndirectSlot {
                                parent_ind_idx: tib_idx,
                                slot_idx: tib_slot,
                            },
                            dib_idx,
                        ));
                        for (dib_slot, sib) in
                            read_slots(&guard.backing, dib).into_iter().enumerate()
                        {
                            if sib != 0 {
                                let sib_idx = indirect_blocks.len();
                                indirect_blocks.push(sib);
                                deferred_inds.push((
                                    sib,
                                    UnitOwner::IndirectSlot {
                                        parent_ind_idx: dib_idx,
                                        slot_idx: dib_slot,
                                    },
                                    sib_idx,
                                ));
                                for (slot_idx, raw) in
                                    read_slots(&guard.backing, sib).into_iter().enumerate()
                                {
                                    push_leaf(
                                        raw,
                                        UnitOwner::IndirectSlot {
                                            parent_ind_idx: sib_idx,
                                            slot_idx,
                                        },
                                        &mut next_target,
                                        &mut units,
                                    );
                                }
                            }
                        }
                    }
                }
            }

            for (ind_blk, owner, ind_idx) in deferred_inds {
                let target = next_target;
                next_target += 1;
                old_to_new.insert(ind_blk, target);
                units.push(RelocUnit {
                    curr_start: ind_blk,
                    block_count: 1,
                    target_start: target,
                    owner,
                    chunk_entry: None,
                    self_ind_idx: Some(ind_idx),
                });
            }
        }

        let mut pending: Vec<usize> = units
            .iter()
            .enumerate()
            .filter_map(|(idx, u)| (u.curr_start != u.target_start).then_some(idx))
            .collect();

        if pending.is_empty() {
            return Ok(0);
        }

        // Initialize compact in-memory mirror of data bitmap allocation state.
        let mut alloc_bm_bytes = guard
            .data_bitmap_slice()
            .map(|s| s.to_vec())
            .unwrap_or_else(|| vec![0u8; total_data.div_ceil(8)]);

        let is_range_free = |bm_bytes: &[u8], start: u64, count: u64| -> bool {
            let Some(rel) = start.checked_sub(db_start).map(|r| r as usize) else {
                return false;
            };
            let cnt = count as usize;
            let Some(end) = rel.checked_add(cnt) else {
                return false;
            };
            if end > total_data {
                return false;
            }
            let bm = crate::bitmap::BitmapRef::new(bm_bytes);
            (rel..end).all(|i| !bm.get(i))
        };

        let find_free_run_high_to_low = |bm_bytes: &[u8], count: u64| -> Option<u64> {
            let cnt = count as usize;
            if cnt == 0 || cnt > total_data {
                return None;
            }
            let bm = crate::bitmap::BitmapRef::new(bm_bytes);
            let mut run = 0usize;
            for idx in (0..total_data).rev() {
                if !bm.get(idx) {
                    run += 1;
                    if run == cnt {
                        return Some(db_start + idx as u64);
                    }
                } else {
                    run = 0;
                }
            }
            None
        };

        let ring_bytes = {
            let mut region = guard.backing.journal_region_mut(&sb)?;
            region.mark_clean();
            let ring = crate::journal::JournalRing::open(&mut region, bs)?;
            ring.ring_bytes() as usize
        };
        let max_payload_per_tx = (ring_bytes.saturating_sub(128)) / 2;

        let mut bytes_moved = 0u64;
        let is_range_based = guard.durability_mode().is_range_based();
        let is_strict = guard.durability_mode() == DurabilityMode::Strict;

        while !pending.is_empty() {
            // Step A: Select a COW batch whose targets are currently free and whose
            // WAL transaction fits comfortably within half the journal ring.
            let mut round_moves: Vec<(usize, u64)> = Vec::new();
            let mut est_tx_bytes = crate::journal::TX_HEADER_LEN + 4;
            let mut touched_inode_set: FxHashSet<usize> = FxHashSet::default();

            for &u_idx in &pending {
                let u = &units[u_idx];
                if !is_range_free(&alloc_bm_bytes, u.target_start, u.block_count) {
                    continue;
                }
                let mut delta = 28usize; // alloc + free SetDataBitmapRange
                match u.owner {
                    UnitOwner::InodeDirect { inode_idx, .. }
                    | UnitOwner::InodeSib { inode_idx }
                    | UnitOwner::InodeDib { inode_idx }
                    | UnitOwner::InodeTib { inode_idx } => {
                        if !touched_inode_set.contains(&inode_idx) {
                            delta += 1 + 8 + crate::journal::INODE_BYTES;
                        }
                    }
                    UnitOwner::IndirectSlot { .. } => {
                        delta += 1 + 8 + 4 + 4 + 8;
                    }
                }
                if u.self_ind_idx.is_some() {
                    delta += 1 + 8 + 4 + 4 + BLOCK_SIZE;
                }
                if est_tx_bytes + delta > max_payload_per_tx && !round_moves.is_empty() {
                    break;
                }
                if let UnitOwner::InodeDirect { inode_idx, .. }
                | UnitOwner::InodeSib { inode_idx }
                | UnitOwner::InodeDib { inode_idx }
                | UnitOwner::InodeTib { inode_idx } = u.owner
                {
                    touched_inode_set.insert(inode_idx);
                }
                est_tx_bytes += delta;
                round_moves.push((u_idx, u.target_start));
            }

            // Cycle breaker: move one blocking unit to a free scratch run.
            if round_moves.is_empty() {
                let u_idx = pending
                    .iter()
                    .copied()
                    .min_by_key(|&idx| units[idx].block_count)
                    .unwrap();
                let count = units[u_idx].block_count;
                let scratch_start =
                    find_free_run_high_to_low(&alloc_bm_bytes, count).ok_or_else(|| {
                        DiskManagerError::Io(std::io::Error::other(
                            "Insufficient contiguous free blocks for WAL-atomic in-place defragmentation",
                        ))
                    })?;
                round_moves.push((u_idx, scratch_start));
            }

            // Step B: Copy payload blocks to free destinations and flush them before WAL commit (Strict mode).
            let mut dst_ranges: Vec<(usize, usize)> = if is_range_based {
                Vec::with_capacity(round_moves.len())
            } else {
                Vec::new()
            };
            for &(u_idx, dst_start) in &round_moves {
                let u = &units[u_idx];
                let dst_off = dst_start as usize * BLOCK_SIZE;
                let len = u.block_count as usize * BLOCK_SIZE;
                for b in 0..u.block_count {
                    guard.backing.copy_block(u.curr_start + b, dst_start + b)?;
                }
                if is_range_based {
                    dst_ranges.push((dst_off, len));
                }
                bytes_moved += len as u64;
            }
            if is_strict {
                for (off, len) in
                    DiskManagerInner::coalesce_ranges(&dst_ranges, guard.backing.len())
                {
                    guard.backing.flush_range(off, len)?;
                }
            }

            // Step C: Stage WAL metadata ops without modifying live metadata yet.
            let mut round_ind_new_block: FxHashMap<usize, u64> = FxHashMap::default();
            for &(u_idx, dst_start) in &round_moves {
                if let Some(ind_idx) = units[u_idx].self_ind_idx {
                    round_ind_new_block.insert(ind_idx, dst_start);
                }
            }

            let mut touched_inodes: FxHashMap<usize, Inode> = FxHashMap::default();
            let mut wal_ops: Vec<crate::journal::MetadataOp> = Vec::new();

            for &(u_idx, dst_start) in &round_moves {
                let u = &units[u_idx];
                let new_slot_val = match u.chunk_entry {
                    Some(entry) => {
                        crate::inode::ChunkEntry::new(
                            dst_start as u32,
                            entry.block_count(),
                            entry.flags(),
                            entry.compressed_len(),
                        )
                        .0
                    }
                    None => dst_start,
                };

                match u.owner {
                    UnitOwner::InodeDirect { inode_idx, slot } => {
                        let inode = touched_inodes
                            .entry(inode_idx)
                            .or_insert_with(|| inodes[inode_idx].1);
                        inode.blocks[slot] = new_slot_val;
                    }
                    UnitOwner::InodeSib { inode_idx } => {
                        let inode = touched_inodes
                            .entry(inode_idx)
                            .or_insert_with(|| inodes[inode_idx].1);
                        inode.blocks[10] = new_slot_val;
                    }
                    UnitOwner::InodeDib { inode_idx } => {
                        let inode = touched_inodes
                            .entry(inode_idx)
                            .or_insert_with(|| inodes[inode_idx].1);
                        inode.blocks[11] = new_slot_val;
                    }
                    UnitOwner::InodeTib { inode_idx } => {
                        let inode = touched_inodes
                            .entry(inode_idx)
                            .or_insert_with(|| inodes[inode_idx].1);
                        inode.triple_indirect = new_slot_val;
                    }
                    UnitOwner::IndirectSlot {
                        parent_ind_idx,
                        slot_idx,
                    } => {
                        let target_ind_blk = round_ind_new_block
                            .get(&parent_ind_idx)
                            .copied()
                            .unwrap_or(indirect_blocks[parent_ind_idx]);
                        crate::journal::push_block_slice_op(
                            &mut wal_ops,
                            target_ind_blk,
                            (slot_idx * 8) as u32,
                            &new_slot_val.to_le_bytes(),
                        );
                    }
                }

                crate::journal::push_data_bitmap_op(
                    &mut wal_ops,
                    dst_start,
                    u.block_count as u32,
                    true,
                );
                crate::journal::push_data_bitmap_op(
                    &mut wal_ops,
                    u.curr_start,
                    u.block_count as u32,
                    false,
                );
            }

            for (&inode_idx, inode) in &touched_inodes {
                let inode_id = inodes[inode_idx].0;
                let slot = Self::encode_inode_slot(guard, inode_id, inode)?;
                wal_ops.push(crate::journal::MetadataOp::WriteInode {
                    inode_id,
                    inode_bytes: Box::new(slot),
                });
            }

            let wal_ops = if crate::journal::is_already_coalesced(&wal_ops) {
                wal_ops
            } else {
                crate::journal::coalesce_ops(wal_ops)
            };

            // Step D: Commit single bounded WAL transaction governed by DurabilityMode.
            Self::commit_journal_tx(guard, &wal_ops)?;

            // Step E: Apply metadata updates in-place via the exact same WAL op applier.
            Self::apply_journal_ops(guard, &wal_ops)?;

            for (&inode_idx, new_inode) in &touched_inodes {
                let inode_id = inodes[inode_idx].0;
                inodes[inode_idx].1 = *new_inode;
                guard.inode_cache.insert(inode_id, *new_inode);
            }

            {
                let mut bm = crate::bitmap::Bitmap::new(&mut alloc_bm_bytes);
                for &(u_idx, dst_start) in &round_moves {
                    let u = &mut units[u_idx];
                    let old_rel = (u.curr_start - db_start) as usize;
                    let new_rel = (dst_start - db_start) as usize;
                    let cnt = u.block_count as usize;
                    bm.clear_range(old_rel, cnt);
                    bm.set_range(new_rel, cnt);

                    u.curr_start = dst_start;
                    if let Some(entry) = u.chunk_entry {
                        u.chunk_entry = Some(crate::inode::ChunkEntry::new(
                            dst_start as u32,
                            entry.block_count(),
                            entry.flags(),
                            entry.compressed_len(),
                        ));
                    }
                    if let Some(ind_idx) = u.self_ind_idx {
                        indirect_blocks[ind_idx] = dst_start;
                    }
                }
            }

            pending.retain(|&idx| units[idx].curr_start != units[idx].target_start);

            // Step F: Sync metadata ranges according to DurabilityMode and retire WAL ring before next round.
            if is_range_based {
                let mut meta_ranges: Vec<(usize, usize)> = vec![guard.data_bitmap_byte_range()];
                if !is_strict {
                    meta_ranges.extend(dst_ranges);
                }
                for &inode_idx in touched_inodes.keys() {
                    meta_ranges.push(guard.inode_byte_range(inodes[inode_idx].0));
                }
                for op in &wal_ops {
                    if let crate::journal::MetadataOp::WriteBlockSlice { block_id, .. } = op {
                        meta_ranges.push(guard.block_byte_range(*block_id));
                    }
                }
                guard.sync_mutation_ranges(&meta_ranges)?;
            }
            {
                let mut region = guard.backing.journal_region_mut(&sb)?;
                region.mark_header_only();
                let mut ring = crate::journal::JournalRing::open(&mut region, bs)?;
                ring.checkpoint();
            }
        }

        guard.free_block_hint = next_target;
        Self::remap_dir_cache_and_clear_payloads(guard, &old_to_new);
        Ok(bytes_moved)
    }

    fn remap_dir_cache_and_clear_payloads(
        guard: &mut DiskManagerInner,
        old_to_new: &FxHashMap<u64, u64>,
    ) {
        guard.dir_cache.for_each_mut(|ix| {
            for (_, blk, _) in ix.stored.values_mut() {
                if let Some(&new_blk) = old_to_new.get(blk) {
                    *blk = new_blk;
                }
            }
            if let Some((tb, off)) = ix.tail_offset
                && let Some(&new_tb) = old_to_new.get(&tb)
            {
                ix.tail_offset = Some((new_tb, off));
            }
        });
        guard.clear_payload_caches();
    }

    /// Structural consistency check (fsck) for OIFS filesystem
    pub fn verify_integrity(&self) -> Result<FsckReport, DiskManagerError> {
        Self::verify_integrity_on(&self.inner)
    }

    /// Structural consistency check driven by an `Arc<RwLock<DiskManagerInner>>`.
    ///
    /// Performs a single-pass directory & block-tree traversal using compact `u64` bitsets,
    /// `read_inode_uncached` (zero `inode_cache` pollution), and zero-allocation directory
    /// and block-pointer visitors (`for_each_valid_inode_in_block`, `for_each_inode_block`).
    pub(crate) fn verify_integrity_on(
        inner: &Arc<RwLock<DiskManagerInner>>,
    ) -> Result<FsckReport, DiskManagerError> {
        let guard = inner.read().unwrap();
        let sb = guard.superblock;

        let max_inodes = sb.inode_count as usize;
        let db_start = sb.data_block_start;
        let max_data_blocks = sb.block_count.saturating_sub(db_start) as usize;

        let inode_words = max_inodes.div_ceil(64);
        let data_words = max_data_blocks.div_ceil(64);

        let mut ref_inodes = vec![0u64; inode_words];
        let mut seen_data = vec![0u64; data_words];
        let mut cross_linked_data = vec![0u64; data_words];
        let mut out_of_range_ref_inodes: FxHashSet<u64> = FxHashSet::default();
        let mut out_of_range_seen_blocks: FxHashSet<u64> = FxHashSet::default();
        let mut out_of_range_cross_linked: FxHashSet<u64> = FxHashSet::default();

        let mut mark_inode_referenced = |inode_id: u64| -> bool {
            if let Ok(idx) = usize::try_from(inode_id)
                && idx < max_inodes
            {
                let mask = 1u64 << (idx % 64);
                let word = &mut ref_inodes[idx / 64];
                let is_new = (*word & mask) == 0;
                *word |= mask;
                is_new
            } else {
                out_of_range_ref_inodes.insert(inode_id)
            }
        };

        let mut record_inode_blocks = |inode: &Inode| {
            Self::for_each_inode_block(&guard.backing, inode, |blk| {
                if blk >= db_start && (blk - db_start) < max_data_blocks as u64 {
                    let idx = (blk - db_start) as usize;
                    let mask = 1u64 << (idx % 64);
                    let sw = &mut seen_data[idx / 64];
                    if (*sw & mask) != 0 {
                        cross_linked_data[idx / 64] |= mask;
                    } else {
                        *sw |= mask;
                    }
                } else if !out_of_range_seen_blocks.insert(blk) {
                    out_of_range_cross_linked.insert(blk);
                }
            });
        };

        // Single-pass reachable tree walk starting from root_inode
        let mut dir_queue: Vec<Inode> = Vec::new();
        mark_inode_referenced(sb.root_inode);
        let root_inode = Self::read_inode_uncached(&guard, sb.root_inode)?;
        record_inode_blocks(&root_inode);
        if root_inode.mode == crate::inode::FileType::Directory {
            dir_queue.push(root_inode);
        }

        while let Some(dir_inode) = dir_queue.pop() {
            let num_blocks = Self::dir_num_blocks(&dir_inode);
            for blk_idx in 0..num_blocks {
                let phys_blk = Self::resolve_logical_block_id(&guard.backing, &dir_inode, blk_idx);
                if phys_blk == 0 {
                    continue;
                }
                let _ = guard.backing.with_block(phys_blk, |block_data| {
                    crate::directory::for_each_valid_inode_in_block(block_data, |child_id| {
                        if mark_inode_referenced(child_id)
                            && let Ok(child_inode) = Self::read_inode_uncached(&guard, child_id)
                        {
                            record_inode_blocks(&child_inode);
                            if child_inode.mode == crate::inode::FileType::Directory {
                                dir_queue.push(child_inode);
                            }
                        }
                    });
                });
            }
        }

        let read_bitmap_word = |slice: &[u8], word_idx: usize, max_bits: usize| -> u64 {
            let byte_start = word_idx * 8;
            if byte_start >= slice.len() {
                return 0;
            }
            let mut buf = [0u8; 8];
            let avail = (slice.len() - byte_start).min(8);
            buf[..avail].copy_from_slice(&slice[byte_start..byte_start + avail]);
            let mut word = u64::from_le_bytes(buf);
            let base_bit = word_idx * 64;
            if max_bits < base_bit + 64 {
                let rem = max_bits.saturating_sub(base_bit);
                word &= if rem == 0 { 0 } else { (1u64 << rem) - 1 };
            }
            word
        };

        // A. Orphan Inodes: allocated in inode bitmap (`1`) & unreferenced (`0`)
        let mut orphan_inodes = Vec::new();
        if let Some(ib_slice) = guard.inode_bitmap_slice() {
            for (w_idx, &ref_w) in ref_inodes.iter().enumerate() {
                let alloc_w = read_bitmap_word(ib_slice, w_idx, max_inodes);
                let mut diff = alloc_w & !ref_w;
                while diff != 0 {
                    let bit = diff.trailing_zeros() as usize;
                    orphan_inodes.push((w_idx * 64 + bit) as u64);
                    diff &= diff - 1;
                }
            }
        }

        // B & C & D. Leaked (`alloc & !seen`), Missing (`seen & !alloc`), and Cross-Linked blocks
        let mut leaked_blocks = Vec::new();
        let mut missing_blocks = Vec::new();
        let mut cross_linked_blocks = Vec::new();
        let db_slice = guard.data_bitmap_slice().unwrap_or(&[]);

        for w_idx in 0..data_words {
            let alloc_w = read_bitmap_word(db_slice, w_idx, max_data_blocks);
            let seen_w = seen_data[w_idx];
            let base_blk = db_start + (w_idx * 64) as u64;

            let mut leaked = alloc_w & !seen_w;
            while leaked != 0 {
                let bit = leaked.trailing_zeros() as u64;
                leaked_blocks.push(base_blk + bit);
                leaked &= leaked - 1;
            }

            let mut missing = seen_w & !alloc_w;
            while missing != 0 {
                let bit = missing.trailing_zeros() as u64;
                missing_blocks.push(base_blk + bit);
                missing &= missing - 1;
            }

            let mut cross = cross_linked_data[w_idx];
            while cross != 0 {
                let bit = cross.trailing_zeros() as u64;
                cross_linked_blocks.push(base_blk + bit);
                cross &= cross - 1;
            }
        }

        if !out_of_range_seen_blocks.is_empty() {
            missing_blocks.extend(out_of_range_seen_blocks);
            missing_blocks.sort_unstable();
        }
        if !out_of_range_cross_linked.is_empty() {
            cross_linked_blocks.extend(out_of_range_cross_linked);
            cross_linked_blocks.sort_unstable();
        }

        let is_clean = orphan_inodes.is_empty()
            && leaked_blocks.is_empty()
            && missing_blocks.is_empty()
            && cross_linked_blocks.is_empty();

        Ok(FsckReport {
            is_clean,
            orphan_inodes,
            leaked_blocks,
            missing_blocks,
            cross_linked_blocks,
        })
    }
}
