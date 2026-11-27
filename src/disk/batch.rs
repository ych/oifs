//! Explicit multi-operation write batch (`WriteBatch`) for `DiskManager`.
//!
//! Coalesces multiple `CreateFileWithData`, `Mkdir`, `Delete`, and `Rename` operations into:
//! - One or a small number of chunked WAL transactions (capped at [`BATCH_WAL_BUDGET`] per
//!   commit so large batches safely span multiple transactions without overflowing the
//!   128 KiB circular WAL ring); or
//! - A single exclusive write lock acquisition with one coalesced range-sync pass on
//!   non-journaled images.

use super::*;
use crate::directory::{
    DirectoryEntry, find_insert_offset_in_block, hash_filename, remove_entry_in_block_bytes,
    validate_filename,
};
use crate::inode::{BlockPath, CHUNK_SIZE_64K, ChunkEntry, FileType, INODE_FLAG_SEEKABLE_64K};
use crate::journal::{
    AllocSim, MetadataOp, coalesce_ops, is_already_coalesced, push_data_bitmap_op,
    sim_get_or_alloc_block, sim_set_logical_chunk_entry, stage_payload_write,
};
use std::collections::BTreeMap;

/// Target metadata byte budget per WAL commit chunk (leaves generous headroom below the
/// 128 KiB journal ring capacity even after worst-case coalesced header/slice expansion).
const BATCH_WAL_BUDGET: usize = 96 * 1024;

/// A single namespace/data mutation staged in a [`WriteBatch`].
#[derive(Debug, Clone)]
pub enum BatchOp {
    /// Create a regular file in `parent_inode_id` and write its initial payload.
    CreateFileWithData {
        parent_inode_id: u64,
        name: String,
        data: Vec<u8>,
        compression_mode: CompressionMode,
        filter_config: crate::filters::FilterConfig,
    },
    /// Create a sub-directory inside `parent_inode_id`.
    Mkdir { parent_inode_id: u64, name: String },
    /// Remove a file or empty directory from `parent_inode_id`.
    Delete { parent_inode_id: u64, name: String },
    /// Atomically rename or move an entry between directories (supporting POSIX overwrite).
    Rename {
        old_parent_inode_id: u64,
        old_name: String,
        new_parent_inode_id: u64,
        new_name: String,
    },
}

/// Summary statistics returned by [`WriteBatch::commit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchCommitResult {
    /// Inode IDs allocated for `CreateFileWithData` and `Mkdir` ops, in batch order.
    pub created_inodes: Vec<u64>,
    /// Total number of operations executed from the batch.
    pub ops_executed: usize,
    /// Number of WAL transactions committed (`0` when journaling is disabled).
    pub wal_transactions: usize,
}

/// Builder for batching multiple filesystem mutations before committing.
pub struct WriteBatch<'a> {
    dm: &'a DiskManager,
    ops: Vec<BatchOp>,
}

impl DiskManager {
    /// Begins a new explicit multi-operation write batch against this mount.
    pub fn begin_write_batch(&self) -> WriteBatch<'_> {
        WriteBatch {
            dm: self,
            ops: Vec::new(),
        }
    }
}

impl<'a> WriteBatch<'a> {
    /// Queues a `CreateFileWithData` operation with default (`FilterConfig::none()`) filters.
    pub fn create_file_with_data(
        &mut self,
        parent_inode_id: u64,
        name: impl Into<String>,
        data: impl Into<Vec<u8>>,
        compression_mode: CompressionMode,
    ) -> &mut Self {
        self.create_file_with_data_with_filters(
            parent_inode_id,
            name,
            data,
            compression_mode,
            crate::filters::FilterConfig::none(),
        )
    }

    /// Queues a `CreateFileWithData` operation with explicit pre-compression filters.
    pub fn create_file_with_data_with_filters(
        &mut self,
        parent_inode_id: u64,
        name: impl Into<String>,
        data: impl Into<Vec<u8>>,
        compression_mode: CompressionMode,
        filter_config: crate::filters::FilterConfig,
    ) -> &mut Self {
        self.ops.push(BatchOp::CreateFileWithData {
            parent_inode_id,
            name: name.into(),
            data: data.into(),
            compression_mode,
            filter_config,
        });
        self
    }

    /// Queues a `Mkdir` operation.
    pub fn mkdir(&mut self, parent_inode_id: u64, name: impl Into<String>) -> &mut Self {
        self.ops.push(BatchOp::Mkdir {
            parent_inode_id,
            name: name.into(),
        });
        self
    }

    /// Queues a `Delete` operation for a file or empty directory.
    pub fn delete(&mut self, parent_inode_id: u64, name: impl Into<String>) -> &mut Self {
        self.ops.push(BatchOp::Delete {
            parent_inode_id,
            name: name.into(),
        });
        self
    }

    /// Queues a POSIX `Rename` operation.
    pub fn rename(
        &mut self,
        old_parent_inode_id: u64,
        old_name: impl Into<String>,
        new_parent_inode_id: u64,
        new_name: impl Into<String>,
    ) -> &mut Self {
        self.ops.push(BatchOp::Rename {
            old_parent_inode_id,
            old_name: old_name.into(),
            new_parent_inode_id,
            new_name: new_name.into(),
        });
        self
    }

    /// Appends a raw [`BatchOp`] to this batch.
    pub fn push(&mut self, op: BatchOp) -> &mut Self {
        self.ops.push(op);
        self
    }

    /// Returns the number of queued operations in this batch.
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// Returns `true` if no operations are currently queued.
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Executes and durably commits all queued operations.
    ///
    /// - Rejects read-only mounts with [`DiskManagerError::ReadOnly`].
    /// - Performs CPU-bound pre-compression completely outside write locks.
    /// - Validates every operation cleanly before committing any transaction, guaranteeing
    ///   zero partial commits on logical errors (missing target, collision, non-empty dir,
    ///   directory cycle, type mismatch).
    pub fn commit(self) -> Result<BatchCommitResult, DiskManagerError> {
        if self.dm.is_read_only() {
            return Err(DiskManagerError::ReadOnly);
        }
        let ops_executed = self.ops.len();
        if ops_executed == 0 {
            return Ok(BatchCommitResult {
                created_inodes: Vec::new(),
                ops_executed: 0,
                wal_transactions: 0,
            });
        }

        // Validate basic syntactic filenames before acquiring locks or compressing payloads.
        for op in &self.ops {
            match op {
                BatchOp::CreateFileWithData { name, .. }
                | BatchOp::Mkdir { name, .. }
                | BatchOp::Delete { name, .. } => validate_name_arg(name)?,
                BatchOp::Rename {
                    old_name, new_name, ..
                } => {
                    validate_name_arg(old_name)?;
                    validate_name_arg(new_name)?;
                }
            }
        }

        // --- STAGE 1 + STAGE 2: Lock-Free Compression ---
        let (dicts, has_encryption) = {
            let guard = self.dm.inner.read().unwrap();
            if guard.read_only {
                return Err(DiskManagerError::ReadOnly);
            }
            (
                Arc::clone(&guard.zstd_dicts),
                guard.encryption_key.is_some(),
            )
        };

        let mut prepared_ops = Vec::with_capacity(self.ops.len());
        let mut est_data_blocks: u64 = 0;
        let mut est_new_inodes: u64 = 0;
        for op in self.ops {
            let prep = prepare_op_stage2(op, &dicts, has_encryption)?;
            match &prep {
                PreparedBatchOp::CreateFileWithData { payload, .. } => {
                    est_new_inodes += 1;
                    let pay_blks = match payload {
                        PreparedPayload::Standard { bytes, .. } => {
                            (bytes.len() + 16).div_ceil(BLOCK_SIZE) as u64
                        }
                        PreparedPayload::Seekable { orig_data, chunks } => {
                            if orig_data.is_empty() {
                                0
                            } else {
                                (orig_data.len() + chunks.len() * 64).div_ceil(BLOCK_SIZE) as u64
                            }
                        }
                    };
                    est_data_blocks += pay_blks + pay_blks.div_ceil(512) + 2;
                }
                PreparedBatchOp::Mkdir { .. } => {
                    est_new_inodes += 1;
                    est_data_blocks += 2;
                }
                PreparedBatchOp::Rename { .. } => {
                    est_data_blocks += 2;
                }
                PreparedBatchOp::Delete { .. } => {}
            }
            prepared_ops.push(prep);
        }

        if est_data_blocks > 0 || est_new_inodes > 0 {
            self.dm
                .ensure_capacity_for(est_data_blocks + 4, est_new_inodes)?;
        }

        // --- STAGE 3: Exclusive Write Lock, Atomic Pre-Validation, & Commit ---
        let mut guard = self.dm.inner.write().unwrap();
        if guard.read_only {
            return Err(DiskManagerError::ReadOnly);
        }

        validate_batch_semantics(&guard, &mut prepared_ops)?;

        let (created_inodes, wal_transactions) = if guard.superblock.has_journal_layout() {
            execute_batch_journaled(&mut guard, prepared_ops)?
        } else {
            let created = execute_batch_in_place(&mut guard, prepared_ops)?;
            (created, 0)
        };

        drop(guard);
        self.dm.maybe_drain_async_ranges();
        Ok(BatchCommitResult {
            created_inodes,
            ops_executed,
            wal_transactions,
        })
    }
}

#[inline]
fn validate_name_arg(name: &str) -> Result<(), DiskManagerError> {
    validate_filename(name).map_err(|e| {
        DiskManagerError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            e.to_string(),
        ))
    })
}

enum PreparedSeekableChunk {
    /// Compressed chunk bytes (encryption deferred until real `inode_id` is allocated).
    Compressed(Vec<u8>),
    /// Uncompressed slice range `[start..start + valid_len)` within `BatchOp` original `data`.
    Raw { start: usize, valid_len: usize },
}

enum PreparedPayload {
    Standard {
        bytes: Vec<u8>,
        is_compressed: bool,
        orig_len: u64,
        filter_config: crate::filters::FilterConfig,
    },
    Seekable {
        orig_data: Vec<u8>,
        chunks: Vec<PreparedSeekableChunk>,
    },
}

enum PreparedBatchOp {
    CreateFileWithData {
        parent_inode_id: u64,
        name: String,
        stored_name: String,
        stored_hash: u64,
        payload: PreparedPayload,
    },
    Mkdir {
        parent_inode_id: u64,
        name: String,
        stored_name: String,
        stored_hash: u64,
    },
    Delete {
        parent_inode_id: u64,
        name: String,
        stored_name: String,
        stored_hash: u64,
    },
    Rename {
        old_parent_inode_id: u64,
        old_name: String,
        old_stored_name: String,
        old_stored_hash: u64,
        new_parent_inode_id: u64,
        new_name: String,
        new_stored_name: String,
        new_stored_hash: u64,
    },
}

fn prepare_op_stage2(
    op: BatchOp,
    dicts: &crate::zstd_pool::ZstdDictRegistry,
    has_encryption: bool,
) -> Result<PreparedBatchOp, DiskManagerError> {
    match op {
        BatchOp::CreateFileWithData {
            parent_inode_id,
            name,
            data,
            compression_mode,
            filter_config,
        } => {
            if let CompressionMode::Seekable { level, .. } = compression_mode {
                let eff_level = if level <= 0 { 1 } else { level };
                let cdict = dicts.default_cdict(eff_level);
                let total_chunks = if data.is_empty() {
                    0
                } else {
                    data.len().div_ceil(CHUNK_SIZE_64K)
                };
                let chunks = if total_chunks > 1 {
                    let slots: Vec<
                        std::cell::UnsafeCell<
                            Option<Result<PreparedSeekableChunk, DiskManagerError>>,
                        >,
                    > = (0..total_chunks)
                        .map(|_| std::cell::UnsafeCell::new(None))
                        .collect();
                    struct SyncSlots<T>(Vec<std::cell::UnsafeCell<Option<T>>>);
                    // SAFETY: `par_for_index` assigns each index `i` at most once cleanly.
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
                    let data_ref = &data;
                    let cd_ref = cdict.as_deref();
                    crate::zstd_pool::par_for_index(total_chunks, |c_idx| {
                        let start = c_idx * CHUNK_SIZE_64K;
                        let valid_len = (data_ref.len() - start).min(CHUNK_SIZE_64K);
                        let res = prepare_single_seekable_stage2(
                            &data_ref[start..start + valid_len],
                            start,
                            valid_len,
                            eff_level,
                            cd_ref,
                            has_encryption,
                        );
                        // SAFETY: index `c_idx` is exclusively written by this invocation.
                        unsafe {
                            sync_slots.set(c_idx, res);
                        }
                    });

                    let mut out = Vec::with_capacity(total_chunks);
                    for cell in sync_slots.0 {
                        out.push(cell.into_inner().expect("every chunk slot populated")?);
                    }
                    out
                } else if total_chunks == 1 {
                    let valid_len = data.len();
                    vec![prepare_single_seekable_stage2(
                        &data,
                        0,
                        valid_len,
                        eff_level,
                        cdict.as_deref(),
                        has_encryption,
                    )?]
                } else {
                    Vec::new()
                };

                Ok(PreparedBatchOp::CreateFileWithData {
                    parent_inode_id,
                    name,
                    stored_name: String::new(),
                    stored_hash: 0,
                    payload: PreparedPayload::Seekable {
                        orig_data: data,
                        chunks,
                    },
                })
            } else {
                let cdict = DiskManager::resolve_default_cdict(dicts, compression_mode);
                let orig_len = data.len() as u64;
                let prep = DiskManager::build_full_overwrite_buffer_pure_with_dict(
                    &data,
                    compression_mode,
                    &filter_config,
                    None,
                    &[],
                    cdict.as_deref(),
                )?;
                let is_compressed = prep.is_compressed;
                let bytes = match prep.buffer {
                    std::borrow::Cow::Owned(v) => v,
                    std::borrow::Cow::Borrowed(_) => data,
                };
                Ok(PreparedBatchOp::CreateFileWithData {
                    parent_inode_id,
                    name,
                    stored_name: String::new(),
                    stored_hash: 0,
                    payload: PreparedPayload::Standard {
                        bytes,
                        is_compressed,
                        orig_len,
                        filter_config,
                    },
                })
            }
        }
        BatchOp::Mkdir {
            parent_inode_id,
            name,
        } => Ok(PreparedBatchOp::Mkdir {
            parent_inode_id,
            name,
            stored_name: String::new(),
            stored_hash: 0,
        }),
        BatchOp::Delete {
            parent_inode_id,
            name,
        } => Ok(PreparedBatchOp::Delete {
            parent_inode_id,
            name,
            stored_name: String::new(),
            stored_hash: 0,
        }),
        BatchOp::Rename {
            old_parent_inode_id,
            old_name,
            new_parent_inode_id,
            new_name,
        } => Ok(PreparedBatchOp::Rename {
            old_parent_inode_id,
            old_name,
            old_stored_name: String::new(),
            old_stored_hash: 0,
            new_parent_inode_id,
            new_name,
            new_stored_name: String::new(),
            new_stored_hash: 0,
        }),
    }
}

fn prepare_single_seekable_stage2(
    slice: &[u8],
    start: usize,
    valid_len: usize,
    level: i32,
    cdict: Option<&zstd::zstd_safe::CDict<'_>>,
    has_encryption: bool,
) -> Result<PreparedSeekableChunk, DiskManagerError> {
    let compressed = if let Some(cd) = cdict {
        crate::zstd_pool::compress_frame_with_cdict(slice, cd).map_err(DiskManagerError::Io)?
    } else {
        crate::zstd_pool::compress_frame(slice, level).map_err(DiskManagerError::Io)?
    };
    let fits = if has_encryption {
        let total_enc_len = 24 + compressed.len() + 16;
        total_enc_len < valid_len && total_enc_len <= 65535
    } else {
        compressed.len() < valid_len && compressed.len() < 65536
    };
    if fits {
        Ok(PreparedSeekableChunk::Compressed(compressed))
    } else {
        Ok(PreparedSeekableChunk::Raw { start, valid_len })
    }
}

// ---------------------------------------------------------------------------
// Pre-Validation Pass (Atomic All-or-Nothing Logical Checks)
// ---------------------------------------------------------------------------

struct SimDirState {
    is_synthetic_dir: bool,
    dir_inode: Option<Inode>,
    /// Uncommitted entries created or moved into this directory during the batch.
    added: FxHashMap<String, (u64, FileType)>,
    /// Entry names removed or moved out of this directory during the batch.
    removed: FxHashSet<String>,
    /// Lazy per-name lookup memo backed by `DiskManager::dir_lookup`.
    lookup_cache: FxHashMap<String, Option<(u64, FileType)>>,
}

impl SimDirState {
    fn synthetic() -> Self {
        Self {
            is_synthetic_dir: true,
            dir_inode: None,
            added: FxHashMap::default(),
            removed: FxHashSet::default(),
            lookup_cache: FxHashMap::default(),
        }
    }

    fn from_disk(dir_inode: Inode) -> Self {
        Self {
            is_synthetic_dir: false,
            dir_inode: Some(dir_inode),
            added: FxHashMap::default(),
            removed: FxHashSet::default(),
            lookup_cache: FxHashMap::default(),
        }
    }
}

struct SimNamespace<'a> {
    guard: &'a DiskManagerInner,
    dirs: FxHashMap<u64, SimDirState>,
    deleted_inodes: FxHashSet<u64>,
    parent_override: FxHashMap<u64, u64>,
    next_synth_id: u64,
}

impl<'a> SimNamespace<'a> {
    fn new(guard: &'a DiskManagerInner) -> Self {
        Self {
            guard,
            dirs: FxHashMap::default(),
            deleted_inodes: FxHashSet::default(),
            parent_override: FxHashMap::default(),
            next_synth_id: u64::MAX - 1,
        }
    }

    fn alloc_synth_id(&mut self) -> u64 {
        let id = self.next_synth_id;
        self.next_synth_id -= 1;
        id
    }

    fn is_synthetic(inode_id: u64) -> bool {
        inode_id > u64::MAX / 2
    }

    fn ensure_dir_valid(&mut self, dir_id: u64) -> Result<(), DiskManagerError> {
        if self.deleted_inodes.contains(&dir_id) {
            return Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Directory not found",
            )));
        }
        if self.dirs.contains_key(&dir_id) {
            return Ok(());
        }
        if Self::is_synthetic(dir_id) {
            return Err(DiskManagerError::Io(std::io::Error::other(
                "Not a directory",
            )));
        }
        if !self.guard.is_inode_allocated(dir_id) {
            return Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Directory not found",
            )));
        }
        let dir_inode = DiskManager::read_inode_internal(self.guard, dir_id)?;
        if dir_inode.mode != FileType::Directory {
            return Err(DiskManagerError::Io(std::io::Error::other(
                "Not a directory",
            )));
        }
        self.dirs.insert(dir_id, SimDirState::from_disk(dir_inode));
        Ok(())
    }

    fn lookup_entry(
        &mut self,
        dir_id: u64,
        name: &str,
    ) -> Result<Option<(u64, FileType)>, DiskManagerError> {
        self.ensure_dir_valid(dir_id)?;
        {
            let st = &self.dirs[&dir_id];
            if let Some(&v) = st.added.get(name) {
                return Ok(Some(v));
            }
            if st.removed.contains(name) || st.is_synthetic_dir {
                return Ok(None);
            }
            if let Some(&cached) = st.lookup_cache.get(name) {
                return Ok(cached);
            }
        }
        let found = match DiskManager::dir_lookup(self.guard, dir_id, name)? {
            Some(child_id) => match DiskManager::read_inode_internal(self.guard, child_id) {
                Ok(child_inode) => Some((child_id, child_inode.mode)),
                Err(_) => None,
            },
            None => None,
        };
        if let Some(st) = self.dirs.get_mut(&dir_id) {
            st.lookup_cache.insert(name.to_string(), found);
        }
        Ok(found)
    }

    fn insert_entry(&mut self, dir_id: u64, name: String, inode_id: u64, mode: FileType) {
        if let Some(st) = self.dirs.get_mut(&dir_id) {
            st.removed.remove(&name);
            st.added.insert(name, (inode_id, mode));
        }
    }

    fn remove_entry(&mut self, dir_id: u64, name: &str) {
        if let Some(st) = self.dirs.get_mut(&dir_id) {
            st.added.remove(name);
            st.removed.insert(name.to_string());
        }
    }

    fn is_dir_empty_sim(&mut self, dir_id: u64) -> Result<bool, DiskManagerError> {
        self.ensure_dir_valid(dir_id)?;
        let (has_added, is_synth, no_removed, dir_inode_opt) = {
            let st = &self.dirs[&dir_id];
            (
                !st.added.is_empty(),
                st.is_synthetic_dir,
                st.removed.is_empty(),
                st.dir_inode.as_ref().cloned(),
            )
        };
        if has_added {
            return Ok(false);
        }
        if is_synth {
            return Ok(true);
        }
        if no_removed {
            return DiskManager::is_dir_empty(self.guard, dir_inode_opt.as_ref().unwrap());
        }
        let list = DiskManager::list_dir_with_guard(self.guard, dir_id)?;
        let st = &self.dirs[&dir_id];
        Ok(!list.into_iter().any(|e| !st.removed.contains(&e.name)))
    }

    fn check_cycle(&self, src_dir_id: u64, new_parent_id: u64) -> Result<bool, DiskManagerError> {
        if src_dir_id == new_parent_id {
            return Ok(true);
        }
        // Walk simulated parent overrides first if `new_parent_id` or its ancestors were
        // created/moved inside this batch.
        let mut cur = new_parent_id;
        let mut seen = FxHashSet::default();
        while seen.insert(cur) {
            if cur == src_dir_id {
                return Ok(true);
            }
            if let Some(&p) = self.parent_override.get(&cur) {
                cur = p;
                continue;
            }
            if Self::is_synthetic(cur) || Self::is_synthetic(src_dir_id) {
                return Ok(false);
            }
            return DiskManager::is_ancestor_dir(self.guard, src_dir_id, cur);
        }
        Ok(false)
    }
}

fn validate_batch_semantics(
    guard: &DiskManagerInner,
    ops: &mut [PreparedBatchOp],
) -> Result<(), DiskManagerError> {
    let mut sim = SimNamespace::new(guard);
    let mut delta_inodes: i64 = 0;

    for op in ops.iter_mut() {
        match op {
            PreparedBatchOp::CreateFileWithData {
                parent_inode_id,
                name,
                stored_name,
                stored_hash,
                ..
            } => {
                if sim.lookup_entry(*parent_inode_id, name)?.is_some() {
                    return Err(DiskManagerError::Io(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        format!("File '{}' already exists", name),
                    )));
                }
                let stored =
                    DiskManager::stored_filename(guard, *parent_inode_id, name)?.into_owned();
                validate_name_arg(&stored)?;
                *stored_hash = hash_filename(&stored);
                *stored_name = stored;
                let sid = sim.alloc_synth_id();
                sim.insert_entry(*parent_inode_id, name.clone(), sid, FileType::File);
                sim.parent_override.insert(sid, *parent_inode_id);
                delta_inodes += 1;
            }
            PreparedBatchOp::Mkdir {
                parent_inode_id,
                name,
                stored_name,
                stored_hash,
            } => {
                if sim.lookup_entry(*parent_inode_id, name)?.is_some() {
                    return Err(DiskManagerError::Io(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        format!("File '{}' already exists", name),
                    )));
                }
                let stored =
                    DiskManager::stored_filename(guard, *parent_inode_id, name)?.into_owned();
                validate_name_arg(&stored)?;
                *stored_hash = hash_filename(&stored);
                *stored_name = stored;
                let sid = sim.alloc_synth_id();
                sim.insert_entry(*parent_inode_id, name.clone(), sid, FileType::Directory);
                sim.dirs.insert(sid, SimDirState::synthetic());
                sim.parent_override.insert(sid, *parent_inode_id);
                delta_inodes += 1;
            }
            PreparedBatchOp::Delete {
                parent_inode_id,
                name,
                stored_name,
                stored_hash,
            } => {
                let Some((target_id, mode)) = sim.lookup_entry(*parent_inode_id, name)? else {
                    return Err(DiskManagerError::Io(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "File not found",
                    )));
                };
                if mode == FileType::Directory && !sim.is_dir_empty_sim(target_id)? {
                    return Err(DiskManagerError::Io(std::io::Error::other(
                        "Directory is not empty",
                    )));
                }
                let stored =
                    DiskManager::stored_filename(guard, *parent_inode_id, name)?.into_owned();
                *stored_hash = hash_filename(&stored);
                *stored_name = stored;
                sim.remove_entry(*parent_inode_id, name);
                sim.deleted_inodes.insert(target_id);
                delta_inodes -= 1;
            }
            PreparedBatchOp::Rename {
                old_parent_inode_id,
                old_name,
                old_stored_name,
                old_stored_hash,
                new_parent_inode_id,
                new_name,
                new_stored_name,
                new_stored_hash,
            } => {
                let Some((src_id, src_mode)) = sim.lookup_entry(*old_parent_inode_id, old_name)?
                else {
                    return Err(DiskManagerError::Io(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "File not found",
                    )));
                };
                if *old_parent_inode_id == *new_parent_inode_id && old_name == new_name {
                    continue;
                }
                let old_stored =
                    DiskManager::stored_filename(guard, *old_parent_inode_id, old_name)?
                        .into_owned();
                let new_stored =
                    DiskManager::stored_filename(guard, *new_parent_inode_id, new_name)?
                        .into_owned();
                validate_name_arg(&new_stored)?;
                *old_stored_hash = hash_filename(&old_stored);
                *old_stored_name = old_stored;
                *new_stored_hash = hash_filename(&new_stored);
                *new_stored_name = new_stored;

                if src_mode == FileType::Directory
                    && sim.check_cycle(src_id, *new_parent_inode_id)?
                {
                    return Err(DiskManagerError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "Cannot move a directory into itself or its subdirectory",
                    )));
                }
                if let Some((dst_id, dst_mode)) =
                    sim.lookup_entry(*new_parent_inode_id, new_name)?
                {
                    if dst_id == src_id {
                        continue;
                    }
                    match (src_mode, dst_mode) {
                        (FileType::Directory, m) if m != FileType::Directory => {
                            return Err(DiskManagerError::Io(std::io::Error::other(
                                "Not a directory",
                            )));
                        }
                        (m, FileType::Directory) if m != FileType::Directory => {
                            return Err(DiskManagerError::Io(std::io::Error::other(
                                "Is a directory",
                            )));
                        }
                        (FileType::Directory, FileType::Directory)
                            if !sim.is_dir_empty_sim(dst_id)? =>
                        {
                            return Err(DiskManagerError::Io(std::io::Error::other(
                                "Directory is not empty",
                            )));
                        }
                        _ => {}
                    }
                    sim.deleted_inodes.insert(dst_id);
                    delta_inodes -= 1;
                }
                sim.remove_entry(*old_parent_inode_id, old_name);
                sim.insert_entry(*new_parent_inode_id, new_name.clone(), src_id, src_mode);
                sim.parent_override.insert(src_id, *new_parent_inode_id);
            }
        }
    }

    // Coarse net inode headroom verification so multi-chunk batches fail up front if full.
    if delta_inodes > 0 {
        let ib_slice = guard
            .inode_bitmap_slice()
            .ok_or_else(|| DiskManagerError::Io(std::io::Error::other("inode bitmap not found")))?;
        let usable = guard.superblock.usable_inode_count() as usize;
        let allocated_cnt: usize = ib_slice
            .as_chunks::<8>()
            .0
            .iter()
            .take(usable / 64)
            .map(|w| u64::from_le_bytes(*w).count_ones() as usize)
            .sum();
        let rem_bits = usable % 64;
        let tail_allocated = if rem_bits > 0 {
            let start = (usable / 64) * 8;
            let mut buf = [0u8; 8];
            let avail = (ib_slice.len().saturating_sub(start)).min(8);
            buf[..avail].copy_from_slice(&ib_slice[start..start + avail]);
            let mask = (1u64 << rem_bits) - 1;
            (u64::from_le_bytes(buf) & mask).count_ones() as usize
        } else {
            0
        };
        let free_inodes = usable.saturating_sub(allocated_cnt + tail_allocated);
        if (delta_inodes as usize) > free_inodes {
            return Err(DiskManagerError::Allocator(
                crate::allocator::AllocatorError::NoSpace,
            ));
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Journaled Chunked Execution (`JournalBatchOverlay`)
// ---------------------------------------------------------------------------

struct JournalBatchOverlay {
    sim: AllocSim,
    inodes: FxHashMap<u64, Inode>,
    dir_blocks: BTreeMap<u64, Vec<u8>>,
    ops: Vec<MetadataOp>,
    payload_ranges: Vec<(usize, usize)>,
    freed_inodes: FxHashSet<u64>,
    touched_dirs: FxHashSet<u64>,
    est_wal_bytes: usize,
    wal_transactions: usize,
}

impl JournalBatchOverlay {
    fn new(guard: &DiskManagerInner) -> Result<Self, DiskManagerError> {
        let sim = AllocSim::new(guard, guard.free_inode_hint, guard.free_block_hint)?;
        Ok(Self {
            sim,
            inodes: FxHashMap::default(),
            dir_blocks: BTreeMap::new(),
            ops: Vec::new(),
            payload_ranges: Vec::new(),
            freed_inodes: FxHashSet::default(),
            touched_dirs: FxHashSet::default(),
            est_wal_bytes: 0,
            wal_transactions: 0,
        })
    }

    fn read_inode(&self, guard: &DiskManagerInner, id: u64) -> Result<Inode, DiskManagerError> {
        if let Some(inode) = self.inodes.get(&id) {
            return Ok(*inode);
        }
        DiskManager::read_inode_internal(guard, id)
    }

    fn write_inode(&mut self, id: u64, inode: Inode) {
        self.freed_inodes.remove(&id);
        let is_new = !self.inodes.contains_key(&id);
        self.inodes.insert(id, inode);
        if is_new {
            self.est_wal_bytes += 280;
        }
    }

    #[inline]
    fn resolve_block(&self, backing: &ImageBacking, inode: &Inode, blk_idx: usize) -> u64 {
        let follow = |parent: u64, slot: usize| -> u64 {
            if parent == 0 {
                0
            } else if let Some(&v) = self.sim.pointers.get(&(parent, slot)) {
                v
            } else {
                backing.read_block_ptr(parent, slot)
            }
        };
        match BlockPath::from_logical(blk_idx) {
            Some(BlockPath::Direct(i)) => inode.blocks[i],
            Some(BlockPath::Single(i)) => follow(inode.blocks[10], i),
            Some(BlockPath::Double(a, b)) => follow(follow(inode.blocks[11], a), b),
            Some(BlockPath::Triple(a, b, c)) => {
                follow(follow(follow(inode.triple_indirect, a), b), c)
            }
            None => 0,
        }
    }

    fn load_dir_block_mut(
        &mut self,
        backing: &ImageBacking,
        blk: u64,
    ) -> Result<&mut Vec<u8>, DiskManagerError> {
        match self.dir_blocks.entry(blk) {
            std::collections::btree_map::Entry::Occupied(e) => Ok(e.into_mut()),
            std::collections::btree_map::Entry::Vacant(e) => {
                let bytes = backing.with_block(blk, |s| s.to_vec()).map_err(|_| {
                    DiskManagerError::Io(std::io::Error::other("Directory block not found"))
                })?;
                self.est_wal_bytes += BLOCK_SIZE + 32;
                Ok(e.insert(bytes))
            }
        }
    }

    fn locate_entry(
        &self,
        backing: &ImageBacking,
        dir_inode: &Inode,
        stored_name: &str,
        stored_hash: u64,
    ) -> Option<(u64, u64)> {
        let num_blocks = DiskManager::dir_num_blocks(dir_inode);
        for blk_idx in 0..num_blocks {
            let phys_blk = self.resolve_block(backing, dir_inode, blk_idx);
            if phys_blk == 0 {
                continue;
            }
            if let Some(staged) = self.dir_blocks.get(&phys_blk) {
                if let Some(id) = crate::directory::find_entry_in_block_with_hash(
                    staged,
                    stored_name,
                    stored_hash,
                ) {
                    return Some((id, phys_blk));
                }
            } else if let Ok(Some(id)) = backing.with_block(phys_blk, |s| {
                crate::directory::find_entry_in_block_with_hash(s, stored_name, stored_hash)
            }) {
                return Some((id, phys_blk));
            }
        }
        None
    }

    fn insert_dir_entry(
        &mut self,
        guard: &DiskManagerInner,
        parent_inode_id: u64,
        parent_inode: &mut Inode,
        child_inode_id: u64,
        stored_name: &str,
        stored_hash: u64,
    ) -> Result<(), DiskManagerError> {
        let entry = DirectoryEntry {
            inode: child_inode_id,
            hash: stored_hash,
            name: stored_name.to_string(),
        };
        let mut entry_bytes = Vec::with_capacity(20 + entry.name.len());
        {
            let mut cursor = std::io::Cursor::new(&mut entry_bytes);
            entry
                .serialize_into(&mut cursor)
                .map_err(|e| DiskManagerError::Io(std::io::Error::other(e.to_string())))?;
        }
        let needed_space = 20 + entry.name.len();
        let num_blocks = DiskManager::dir_num_blocks(parent_inode);

        let mut slot: Option<(u64, usize)> = None;
        for blk_idx in 0..num_blocks {
            let phys_blk = self.resolve_block(&guard.backing, parent_inode, blk_idx);
            if !self.sim.is_data_block(phys_blk) {
                continue;
            }
            let insert_off = if let Some(staged) = self.dir_blocks.get(&phys_blk) {
                find_insert_offset_in_block(staged)
            } else {
                guard
                    .backing
                    .with_block(phys_blk, find_insert_offset_in_block)
                    .unwrap_or(BLOCK_SIZE)
            };
            if insert_off + needed_space <= BLOCK_SIZE {
                slot = Some((phys_blk, insert_off));
                break;
            }
        }

        if let Some((phys_blk, insert_off)) = slot {
            let blk_buf = self.load_dir_block_mut(&guard.backing, phys_blk)?;
            blk_buf[insert_off..insert_off + entry_bytes.len()].copy_from_slice(&entry_bytes);
            let end = insert_off + entry_bytes.len();
            if end + 18 <= BLOCK_SIZE {
                blk_buf[end..end + 18].fill(0);
            }
        } else {
            let new_blk_idx = num_blocks;
            let pre_len = self.ops.len();
            let phys_blk = sim_get_or_alloc_block(
                &guard.backing,
                parent_inode,
                new_blk_idx,
                &mut self.sim,
                &mut self.ops,
            )?;
            self.est_wal_bytes += (self.ops.len() - pre_len) * 32;
            let mut block_image = vec![0u8; BLOCK_SIZE];
            block_image[..entry_bytes.len()].copy_from_slice(&entry_bytes);
            self.dir_blocks.insert(phys_blk, block_image);
            self.est_wal_bytes += BLOCK_SIZE + 32;
            parent_inode.size = (new_blk_idx + 1) as u64 * BLOCK_SIZE as u64;
        }
        self.touched_dirs.insert(parent_inode_id);
        Ok(())
    }

    fn collect_inode_blocks_with_sim(&self, backing: &ImageBacking, inode: &Inode) -> Vec<u64> {
        let mut blks = Vec::new();
        let read_ptr = |parent: u64, slot: usize| -> u64 {
            if parent == 0 {
                0
            } else if let Some(&v) = self.sim.pointers.get(&(parent, slot)) {
                v
            } else {
                backing.read_block_ptr(parent, slot)
            }
        };
        let is_seekable = (inode.flags & INODE_FLAG_SEEKABLE_64K) != 0;

        fn push_leaf(blks: &mut Vec<u64>, is_seekable: bool, sim: &AllocSim, raw: u64) {
            if raw == 0 {
                return;
            }
            if is_seekable {
                let ce = ChunkEntry(raw);
                let sb = ce.start_block() as u64;
                let cnt = ce.block_count() as u64;
                if !ce.is_empty() && cnt > 0 && sim.is_data_block(sb) {
                    for off in 0..cnt {
                        blks.push(sb + off);
                    }
                }
            } else if sim.is_data_block(raw) {
                blks.push(raw);
            }
        }

        for i in 0..10 {
            push_leaf(&mut blks, is_seekable, &self.sim, inode.blocks[i]);
        }

        let sib_id = inode.blocks[10];
        if sib_id != 0 {
            blks.push(sib_id);
            for slot in 0..512 {
                push_leaf(&mut blks, is_seekable, &self.sim, read_ptr(sib_id, slot));
            }
        }

        let dib_id = inode.blocks[11];
        if dib_id != 0 {
            blks.push(dib_id);
            for a in 0..512 {
                let sib = read_ptr(dib_id, a);
                if sib != 0 {
                    blks.push(sib);
                    for b in 0..512 {
                        push_leaf(&mut blks, is_seekable, &self.sim, read_ptr(sib, b));
                    }
                }
            }
        }

        let tib_id = inode.triple_indirect;
        if tib_id != 0 {
            blks.push(tib_id);
            for a in 0..512 {
                let dib = read_ptr(tib_id, a);
                if dib != 0 {
                    blks.push(dib);
                    for b in 0..512 {
                        let sib = read_ptr(dib, b);
                        if sib != 0 {
                            blks.push(sib);
                            for c in 0..512 {
                                push_leaf(&mut blks, is_seekable, &self.sim, read_ptr(sib, c));
                            }
                        }
                    }
                }
            }
        }
        blks
    }

    fn flush_chunk(&mut self, guard: &mut DiskManagerInner) -> Result<(), DiskManagerError> {
        if self.ops.is_empty() && self.inodes.is_empty() && self.dir_blocks.is_empty() {
            return Ok(());
        }

        let mut ops = std::mem::take(&mut self.ops);
        let dir_blocks = std::mem::take(&mut self.dir_blocks);
        let mut touched_block_ids: Vec<u64> = Vec::with_capacity(dir_blocks.len());
        for (block_id, data) in dir_blocks {
            touched_block_ids.push(block_id);
            ops.push(MetadataOp::WriteBlockSlice {
                block_id,
                offset: 0,
                data,
            });
        }

        let updated_inodes = std::mem::take(&mut self.inodes);
        let mut updated_inode_records = Vec::with_capacity(updated_inodes.len());
        for (inode_id, inode) in updated_inodes {
            let img = DiskManager::build_inode_post_image(guard, inode_id, &inode)?;
            ops.push(MetadataOp::WriteInode {
                inode_id,
                inode_bytes: Box::new(img),
            });
            updated_inode_records.push((inode_id, inode));
        }

        let coalesced = if is_already_coalesced(&ops) {
            ops
        } else {
            coalesce_ops(ops)
        };

        // Ordered mode: flush direct payload blocks before committing WAL metadata.
        if guard.durability_mode() == DurabilityMode::Strict
            && !is_strict_sync_deferred()
            && !self.payload_ranges.is_empty()
        {
            if guard.backing.as_mmap_slice().is_some() {
                let coalesced_payload =
                    DiskManagerInner::coalesce_ranges(&self.payload_ranges, guard.backing.len());
                for (off, actual) in coalesced_payload {
                    guard.backing.flush_range(off, actual)?;
                }
            } else {
                guard.backing.flush()?;
            }
        }

        DiskManager::commit_journal_tx(guard, &coalesced)?;
        DiskManager::apply_journal_ops(guard, &coalesced)?;

        guard.free_inode_hint = self.sim.free_inode_hint;
        guard.free_block_hint = self.sim.free_block_hint;

        let freed = std::mem::take(&mut self.freed_inodes);
        for id in &freed {
            guard.inode_cache.remove(*id);
            guard.dir_cache.remove(*id);
            guard.invalidate_payload_caches(*id);
            guard.bump_inode_gen(*id);
        }
        for (id, inode) in &updated_inode_records {
            if !freed.contains(id) {
                guard.cache_inode(*id, *inode);
                guard.invalidate_payload_caches(*id);
                guard.bump_inode_gen(*id);
            }
        }
        for dir_id in self.touched_dirs.drain() {
            guard.dir_cache.remove(dir_id);
        }

        if guard.durability_mode() == DurabilityMode::RangeAsync {
            let mut ranges = vec![
                guard.inode_bitmap_byte_range(),
                guard.data_bitmap_byte_range(),
            ];
            ranges.append(&mut self.payload_ranges);
            for (id, _) in &updated_inode_records {
                ranges.push(guard.inode_byte_range(*id));
            }
            for blk in touched_block_ids {
                ranges.push(guard.block_byte_range(blk));
            }
            guard.sync_mutation_ranges(&ranges)?;
        } else if guard.durability_mode() == DurabilityMode::LegacyWholeMmapAsync {
            guard.sync_mutation_ranges(&[])?;
        }
        self.payload_ranges.clear();
        DiskManager::maybe_checkpoint(guard)?;

        self.wal_transactions += 1;
        self.est_wal_bytes = 0;
        self.sim = AllocSim::new(guard, guard.free_inode_hint, guard.free_block_hint)?;
        Ok(())
    }
}

#[inline]
pub(crate) const fn should_flush_wal_chunk(cur_est: usize, next_est: usize) -> bool {
    cur_est > 0 && cur_est.saturating_add(next_est) > BATCH_WAL_BUDGET
}

#[inline]
pub(crate) const fn estimate_standard_create_wal_bytes(payload_len: usize) -> usize {
    let blks = payload_len.div_ceil(BLOCK_SIZE);
    360usize.saturating_add(blks.saturating_div(512).saturating_mul(32))
}

#[inline]
pub(crate) const fn estimate_seekable_create_wal_bytes(num_chunks: usize) -> usize {
    360usize.saturating_add(num_chunks.saturating_mul(48))
}

fn estimate_op_wal_bytes(op: &PreparedBatchOp) -> usize {
    match op {
        PreparedBatchOp::CreateFileWithData { payload, .. } => match payload {
            PreparedPayload::Standard { bytes, .. } => {
                estimate_standard_create_wal_bytes(bytes.len())
            }
            PreparedPayload::Seekable { chunks, .. } => {
                estimate_seekable_create_wal_bytes(chunks.len())
            }
        },
        PreparedBatchOp::Mkdir { .. } => 4600,
        PreparedBatchOp::Delete { .. } => 4500,
        PreparedBatchOp::Rename { .. } => 8900,
    }
}

fn execute_batch_journaled(
    guard: &mut DiskManagerInner,
    ops: Vec<PreparedBatchOp>,
) -> Result<(Vec<u64>, usize), DiskManagerError> {
    let mut overlay = JournalBatchOverlay::new(guard)?;
    let mut created_inodes = Vec::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    for op in ops {
        let est = estimate_op_wal_bytes(&op);
        if should_flush_wal_chunk(overlay.est_wal_bytes, est) {
            overlay.flush_chunk(guard)?;
        }

        match op {
            PreparedBatchOp::CreateFileWithData {
                parent_inode_id,
                name: _,
                stored_name,
                stored_hash,
                payload,
            } => {
                let mut parent_inode = overlay.read_inode(guard, parent_inode_id)?;
                let new_inode_id = overlay.sim.alloc_inode()?;
                overlay.ops.push(MetadataOp::SetInodeBitmap {
                    inode_id: new_inode_id,
                    allocated: true,
                });
                overlay.est_wal_bytes += 24;

                let mut new_inode = Inode::new(FileType::File);
                new_inode.created_at = now;
                new_inode.modified_at = now;

                match payload {
                    PreparedPayload::Standard {
                        bytes,
                        is_compressed,
                        orig_len,
                        filter_config,
                    } => {
                        new_inode.filter_typesize = filter_config.typesize;
                        new_inode.filter_delta = filter_config.delta;
                        new_inode.filter_shuffle = filter_config.shuffle;
                        new_inode.filter_bitshuffle = filter_config.bitshuffle;

                        let final_buf = if let Some(key) = guard.encryption_key.as_ref() {
                            let enc_scheme = guard.superblock.encryption_version;
                            let (aad_buf, aad_len) = crate::encryption::file_aad_for_scheme_stack(
                                new_inode_id,
                                enc_scheme,
                            );
                            let nonce = crate::encryption::generate_nonce();
                            let mut enc = bytes;
                            enc.reserve(16);
                            crate::encryption::encrypt_data_in_place_for_scheme(
                                &mut enc,
                                key,
                                &nonce,
                                &aad_buf[..aad_len],
                                enc_scheme,
                            )?;
                            new_inode.encrypted = true;
                            new_inode.encryption_nonce = nonce;
                            enc
                        } else {
                            bytes
                        };

                        new_inode.size = if is_compressed {
                            orig_len
                        } else {
                            final_buf.len() as u64
                        };
                        new_inode.compressed_size = if is_compressed {
                            final_buf.len() as u64
                        } else {
                            0
                        };

                        if !final_buf.is_empty() {
                            let mut used = Vec::new();
                            let pre_ops = overlay.ops.len();
                            stage_payload_write(
                                guard,
                                &mut new_inode,
                                0,
                                &final_buf,
                                &mut overlay.sim,
                                &mut overlay.ops,
                                &mut used,
                            )?;
                            overlay.est_wal_bytes += (overlay.ops.len() - pre_ops) * 40;
                            for blk in used {
                                overlay.payload_ranges.push(guard.block_byte_range(blk));
                            }
                        }
                    }
                    PreparedPayload::Seekable { orig_data, chunks } => {
                        new_inode.flags |= INODE_FLAG_SEEKABLE_64K;
                        new_inode.size = orig_data.len() as u64;
                        let enc_scheme = guard.superblock.encryption_version;
                        if guard.encryption_key.is_some() {
                            new_inode.encrypted = true;
                            new_inode.encryption_nonce = crate::encryption::generate_nonce();
                        }

                        let mut total_comp_bytes: u64 = 0;
                        for (chunk_idx, chunk_stage) in chunks.into_iter().enumerate() {
                            let (payload, flag, comp_len, blocks_needed) =
                                finalize_seekable_chunk_stage3(
                                    chunk_stage,
                                    &orig_data,
                                    guard.encryption_key.as_ref(),
                                    new_inode_id,
                                    chunk_idx as u64,
                                    enc_scheme,
                                )?;
                            total_comp_bytes = total_comp_bytes.saturating_add(comp_len as u64);

                            let pre_ops = overlay.ops.len();
                            let start_blk = overlay
                                .sim
                                .alloc_contiguous(&guard.backing, blocks_needed)?;
                            push_data_bitmap_op(
                                &mut overlay.ops,
                                start_blk,
                                blocks_needed as u32,
                                true,
                            );
                            DiskManager::write_chunk_blocks(
                                &mut guard.backing,
                                start_blk,
                                blocks_needed,
                                &payload,
                            )?;
                            for b_off in 0..blocks_needed {
                                overlay
                                    .payload_ranges
                                    .push(guard.block_byte_range(start_blk + b_off as u64));
                            }
                            let entry = ChunkEntry::new(
                                start_blk as u32,
                                blocks_needed as u8,
                                flag,
                                comp_len,
                            );
                            sim_set_logical_chunk_entry(
                                &guard.backing,
                                &mut new_inode,
                                chunk_idx,
                                entry,
                                &mut overlay.sim,
                                &mut overlay.ops,
                            )?;
                            overlay.est_wal_bytes += (overlay.ops.len() - pre_ops) * 48;
                        }
                        new_inode.compressed_size = total_comp_bytes;
                    }
                }

                overlay.insert_dir_entry(
                    guard,
                    parent_inode_id,
                    &mut parent_inode,
                    new_inode_id,
                    &stored_name,
                    stored_hash,
                )?;
                parent_inode.modified_at = now;
                overlay.write_inode(new_inode_id, new_inode);
                overlay.write_inode(parent_inode_id, parent_inode);
                created_inodes.push(new_inode_id);
            }
            PreparedBatchOp::Mkdir {
                parent_inode_id,
                name: _,
                stored_name,
                stored_hash,
            } => {
                let mut parent_inode = overlay.read_inode(guard, parent_inode_id)?;
                let new_inode_id = overlay.sim.alloc_inode()?;
                overlay.ops.push(MetadataOp::SetInodeBitmap {
                    inode_id: new_inode_id,
                    allocated: true,
                });

                let dir_blk = overlay.sim.alloc_block(&guard.backing)?;
                push_data_bitmap_op(&mut overlay.ops, dir_blk, 1, true);
                overlay.dir_blocks.insert(dir_blk, vec![0u8; BLOCK_SIZE]);
                overlay.est_wal_bytes += BLOCK_SIZE + 64;

                let mut new_dir = Inode::new(FileType::Directory);
                new_dir.created_at = now;
                new_dir.modified_at = now;
                new_dir.blocks[0] = dir_blk;
                new_dir.size = BLOCK_SIZE as u64;

                overlay.insert_dir_entry(
                    guard,
                    parent_inode_id,
                    &mut parent_inode,
                    new_inode_id,
                    &stored_name,
                    stored_hash,
                )?;
                parent_inode.modified_at = now;
                overlay.write_inode(new_inode_id, new_dir);
                overlay.write_inode(parent_inode_id, parent_inode);
                overlay.touched_dirs.insert(new_inode_id);
                created_inodes.push(new_inode_id);
            }
            PreparedBatchOp::Delete {
                parent_inode_id,
                name: _,
                stored_name,
                stored_hash,
            } => {
                let mut parent_inode = overlay.read_inode(guard, parent_inode_id)?;
                let (target_id, phys_blk) = overlay
                    .locate_entry(&guard.backing, &parent_inode, &stored_name, stored_hash)
                    .ok_or_else(|| {
                        DiskManagerError::Io(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "File not found",
                        ))
                    })?;

                let target_inode = overlay.read_inode(guard, target_id)?;
                let blocks_to_free =
                    overlay.collect_inode_blocks_with_sim(&guard.backing, &target_inode);

                {
                    let dir_blk_buf = overlay.load_dir_block_mut(&guard.backing, phys_blk)?;
                    remove_entry_in_block_bytes(dir_blk_buf, &stored_name, stored_hash);
                }

                overlay.sim.free_inode(target_id)?;
                overlay.ops.push(MetadataOp::SetInodeBitmap {
                    inode_id: target_id,
                    allocated: false,
                });
                for blk in blocks_to_free {
                    let _ = overlay.sim.free_contiguous(&guard.backing, blk, 1);
                    push_data_bitmap_op(&mut overlay.ops, blk, 1, false);
                    overlay.dir_blocks.remove(&blk);
                }
                overlay.freed_inodes.insert(target_id);
                overlay.inodes.remove(&target_id);
                overlay.touched_dirs.insert(parent_inode_id);
                overlay.touched_dirs.insert(target_id);

                parent_inode.modified_at = now;
                overlay.write_inode(parent_inode_id, parent_inode);
                overlay.est_wal_bytes += 128;
            }
            PreparedBatchOp::Rename {
                old_parent_inode_id,
                old_name,
                old_stored_name: old_stored,
                old_stored_hash: old_hash,
                new_parent_inode_id,
                new_name,
                new_stored_name: new_stored,
                new_stored_hash: new_hash,
            } => {
                if old_parent_inode_id == new_parent_inode_id && old_name == new_name {
                    continue;
                }
                let mut old_parent = overlay.read_inode(guard, old_parent_inode_id)?;
                let same_parent = old_parent_inode_id == new_parent_inode_id;
                let mut new_parent_opt = if same_parent {
                    None
                } else {
                    Some(overlay.read_inode(guard, new_parent_inode_id)?)
                };

                let (src_id, src_phys_blk) = overlay
                    .locate_entry(&guard.backing, &old_parent, &old_stored, old_hash)
                    .ok_or_else(|| {
                        DiskManagerError::Io(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "File not found",
                        ))
                    })?;

                let dst_loc = {
                    let np_ref = new_parent_opt.as_ref().unwrap_or(&old_parent);
                    overlay.locate_entry(&guard.backing, np_ref, &new_stored, new_hash)
                };
                if let Some((dst_id, _)) = dst_loc
                    && dst_id == src_id
                {
                    continue;
                }

                // 1. Remove old entry
                {
                    let src_buf = overlay.load_dir_block_mut(&guard.backing, src_phys_blk)?;
                    remove_entry_in_block_bytes(src_buf, &old_stored, old_hash);
                }

                // 2. If overwriting existing target, unlink + free its resources.
                if let Some((dst_id, dst_phys_blk)) = dst_loc {
                    let dst_inode = overlay.read_inode(guard, dst_id)?;
                    let dst_blocks =
                        overlay.collect_inode_blocks_with_sim(&guard.backing, &dst_inode);
                    {
                        let dst_buf = overlay.load_dir_block_mut(&guard.backing, dst_phys_blk)?;
                        remove_entry_in_block_bytes(dst_buf, &new_stored, new_hash);
                    }
                    overlay.sim.free_inode(dst_id)?;
                    overlay.ops.push(MetadataOp::SetInodeBitmap {
                        inode_id: dst_id,
                        allocated: false,
                    });
                    for blk in dst_blocks {
                        let _ = overlay.sim.free_contiguous(&guard.backing, blk, 1);
                        push_data_bitmap_op(&mut overlay.ops, blk, 1, false);
                        overlay.dir_blocks.remove(&blk);
                    }
                    overlay.freed_inodes.insert(dst_id);
                    overlay.inodes.remove(&dst_id);
                    overlay.touched_dirs.insert(dst_id);
                }

                // 3. Insert new entry
                {
                    let np_mut = new_parent_opt.as_mut().unwrap_or(&mut old_parent);
                    overlay.insert_dir_entry(
                        guard,
                        new_parent_inode_id,
                        np_mut,
                        src_id,
                        &new_stored,
                        new_hash,
                    )?;
                }

                old_parent.modified_at = now;
                overlay.write_inode(old_parent_inode_id, old_parent);
                if let Some(mut np) = new_parent_opt {
                    np.modified_at = now;
                    overlay.write_inode(new_parent_inode_id, np);
                }
                overlay.touched_dirs.insert(old_parent_inode_id);
                overlay.touched_dirs.insert(new_parent_inode_id);
            }
        }
    }

    overlay.flush_chunk(guard)?;
    Ok((created_inodes, overlay.wal_transactions))
}

fn finalize_seekable_chunk_stage3(
    stage: PreparedSeekableChunk,
    orig_data: &[u8],
    encryption_key: Option<&crate::encryption::EncryptionKey>,
    inode_id: u64,
    chunk_idx: u64,
    scheme: u8,
) -> Result<(Vec<u8>, u8, u16, usize), DiskManagerError> {
    match (stage, encryption_key) {
        (PreparedSeekableChunk::Compressed(compressed), Some(key)) => {
            let nonce = crate::encryption::generate_nonce();
            let (aad_buf, aad_len) = crate::encryption::chunk_aad_for_scheme_stack(
                inode_id,
                chunk_idx,
                ChunkEntry::FLAG_COMPRESSED,
                scheme,
            );
            let payload = crate::encryption::encrypt_with_nonce_prefix_for_scheme(
                &compressed,
                key,
                &nonce,
                &aad_buf[..aad_len],
                scheme,
            )?;
            let blks = payload.len().div_ceil(BLOCK_SIZE);
            let comp_len = payload.len() as u16;
            Ok((payload, ChunkEntry::FLAG_COMPRESSED, comp_len, blks))
        }
        (PreparedSeekableChunk::Raw { start, valid_len }, Some(key)) => {
            let nonce = crate::encryption::generate_nonce();
            let (aad_buf, aad_len) = crate::encryption::chunk_aad_for_scheme_stack(
                inode_id,
                chunk_idx,
                ChunkEntry::FLAG_RAW,
                scheme,
            );
            let payload = crate::encryption::encrypt_with_nonce_prefix_for_scheme(
                &orig_data[start..start + valid_len],
                key,
                &nonce,
                &aad_buf[..aad_len],
                scheme,
            )?;
            let blks = payload.len().div_ceil(BLOCK_SIZE);
            let comp_len = (payload.len().min(65535)) as u16;
            Ok((payload, ChunkEntry::FLAG_RAW, comp_len, blks))
        }
        (PreparedSeekableChunk::Compressed(compressed), None) => {
            let blks = compressed.len().div_ceil(BLOCK_SIZE);
            let comp_len = compressed.len() as u16;
            Ok((compressed, ChunkEntry::FLAG_COMPRESSED, comp_len, blks))
        }
        (PreparedSeekableChunk::Raw { start, valid_len }, None) => {
            let blks = valid_len.div_ceil(BLOCK_SIZE);
            Ok((
                orig_data[start..start + valid_len].to_vec(),
                ChunkEntry::FLAG_RAW,
                valid_len as u16,
                blks,
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Non-Journaled In-Place Execution (Single Write Lock + Single Sync)
// ---------------------------------------------------------------------------

fn execute_batch_in_place(
    guard: &mut DiskManagerInner,
    ops: Vec<PreparedBatchOp>,
) -> Result<Vec<u64>, DiskManagerError> {
    let mut created_inodes = Vec::new();
    let mut dirty_ranges: Vec<(usize, usize)> = Vec::new();
    let mut touched_inode_bm = false;
    let mut touched_data_bm = false;
    let mut touched_dirs: FxHashSet<u64> = FxHashSet::default();
    let track_ranges = guard.durability_mode().is_range_based();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    for op in ops {
        match op {
            PreparedBatchOp::CreateFileWithData {
                parent_inode_id,
                name: _,
                stored_name,
                stored_hash,
                payload,
            } => {
                let mut parent_inode = DiskManager::read_inode_internal(guard, parent_inode_id)?;

                let new_inode_id = allocate_inode_in_place(guard)?;
                touched_inode_bm = true;

                let mut new_inode = Inode::new(FileType::File);
                new_inode.created_at = now;
                new_inode.modified_at = now;

                match payload {
                    PreparedPayload::Standard {
                        bytes,
                        is_compressed,
                        orig_len,
                        filter_config,
                    } => {
                        new_inode.filter_typesize = filter_config.typesize;
                        new_inode.filter_delta = filter_config.delta;
                        new_inode.filter_shuffle = filter_config.shuffle;
                        new_inode.filter_bitshuffle = filter_config.bitshuffle;

                        let final_buf = if let Some(key) = guard.encryption_key.as_ref() {
                            let enc_scheme = guard.superblock.encryption_version;
                            let (aad_buf, aad_len) = crate::encryption::file_aad_for_scheme_stack(
                                new_inode_id,
                                enc_scheme,
                            );
                            let nonce = crate::encryption::generate_nonce();
                            let mut enc = bytes;
                            enc.reserve(16);
                            crate::encryption::encrypt_data_in_place_for_scheme(
                                &mut enc,
                                key,
                                &nonce,
                                &aad_buf[..aad_len],
                                enc_scheme,
                            )?;
                            new_inode.encrypted = true;
                            new_inode.encryption_nonce = nonce;
                            enc
                        } else {
                            bytes
                        };

                        new_inode.size = if is_compressed {
                            orig_len
                        } else {
                            final_buf.len() as u64
                        };
                        new_inode.compressed_size = if is_compressed {
                            final_buf.len() as u64
                        } else {
                            0
                        };

                        if !final_buf.is_empty() {
                            touched_data_bm = true;
                            let mut touched_blks =
                                if track_ranges { Some(Vec::new()) } else { None };
                            DiskManager::write_buffer_at_offset(
                                guard,
                                &mut new_inode,
                                0,
                                &final_buf,
                                touched_blks.as_mut(),
                            )?;
                            if let Some(blks) = touched_blks {
                                for blk in blks {
                                    dirty_ranges.push(guard.block_byte_range(blk));
                                }
                            }
                        }
                    }
                    PreparedPayload::Seekable { orig_data, chunks } => {
                        new_inode.flags |= INODE_FLAG_SEEKABLE_64K;
                        new_inode.size = orig_data.len() as u64;
                        let enc_scheme = guard.superblock.encryption_version;
                        if guard.encryption_key.is_some() {
                            new_inode.encrypted = true;
                            new_inode.encryption_nonce = crate::encryption::generate_nonce();
                        }
                        if !chunks.is_empty() {
                            touched_data_bm = true;
                        }
                        let mut total_comp_bytes: u64 = 0;
                        for (chunk_idx, chunk_stage) in chunks.into_iter().enumerate() {
                            let (payload, flag, comp_len, blocks_needed) =
                                finalize_seekable_chunk_stage3(
                                    chunk_stage,
                                    &orig_data,
                                    guard.encryption_key.as_ref(),
                                    new_inode_id,
                                    chunk_idx as u64,
                                    enc_scheme,
                                )?;
                            total_comp_bytes = total_comp_bytes.saturating_add(comp_len as u64);

                            let start_blk = DiskManager::allocate_contiguous_blocks_internal(
                                guard,
                                blocks_needed,
                            )?;
                            DiskManager::write_chunk_blocks(
                                &mut guard.backing,
                                start_blk,
                                blocks_needed,
                                &payload,
                            )?;
                            if track_ranges {
                                for off in 0..blocks_needed {
                                    dirty_ranges
                                        .push(guard.block_byte_range(start_blk + off as u64));
                                }
                            }
                            let entry = ChunkEntry::new(
                                start_blk as u32,
                                blocks_needed as u8,
                                flag,
                                comp_len,
                            );
                            DiskManager::set_logical_chunk_entry(
                                guard,
                                &mut new_inode,
                                chunk_idx,
                                entry,
                                None,
                            )?;
                        }
                        new_inode.compressed_size = total_comp_bytes;
                    }
                }

                insert_dir_entry_in_place(
                    guard,
                    parent_inode_id,
                    &mut parent_inode,
                    new_inode_id,
                    &stored_name,
                    stored_hash,
                    &mut touched_data_bm,
                    track_ranges.then_some(&mut dirty_ranges),
                )?;
                parent_inode.modified_at = now;
                DiskManager::write_inode_internal(guard, new_inode_id, &new_inode)?;
                DiskManager::write_inode_internal(guard, parent_inode_id, &parent_inode)?;
                if track_ranges {
                    dirty_ranges.push(guard.inode_byte_range(new_inode_id));
                    dirty_ranges.push(guard.inode_byte_range(parent_inode_id));
                }
                touched_dirs.insert(parent_inode_id);
                created_inodes.push(new_inode_id);
            }
            PreparedBatchOp::Mkdir {
                parent_inode_id,
                name: _,
                stored_name,
                stored_hash,
            } => {
                let mut parent_inode = DiskManager::read_inode_internal(guard, parent_inode_id)?;

                let new_inode_id = allocate_inode_in_place(guard)?;
                touched_inode_bm = true;

                let dir_blk = allocate_single_block_in_place(guard)?;
                touched_data_bm = true;
                let zero_blk = [0u8; BLOCK_SIZE];
                guard
                    .backing
                    .write_all_at(dir_blk * BLOCK_SIZE as u64, &zero_blk)?;
                if track_ranges {
                    dirty_ranges.push(guard.block_byte_range(dir_blk));
                }

                let mut new_dir = Inode::new(FileType::Directory);
                new_dir.created_at = now;
                new_dir.modified_at = now;
                new_dir.blocks[0] = dir_blk;
                new_dir.size = BLOCK_SIZE as u64;

                insert_dir_entry_in_place(
                    guard,
                    parent_inode_id,
                    &mut parent_inode,
                    new_inode_id,
                    &stored_name,
                    stored_hash,
                    &mut touched_data_bm,
                    track_ranges.then_some(&mut dirty_ranges),
                )?;
                parent_inode.modified_at = now;
                DiskManager::write_inode_internal(guard, new_inode_id, &new_dir)?;
                DiskManager::write_inode_internal(guard, parent_inode_id, &parent_inode)?;
                if track_ranges {
                    dirty_ranges.push(guard.inode_byte_range(new_inode_id));
                    dirty_ranges.push(guard.inode_byte_range(parent_inode_id));
                }
                touched_dirs.insert(parent_inode_id);
                touched_dirs.insert(new_inode_id);
                created_inodes.push(new_inode_id);
            }
            PreparedBatchOp::Delete {
                parent_inode_id,
                name: _,
                stored_name,
                stored_hash,
            } => {
                let mut parent_inode = DiskManager::read_inode_internal(guard, parent_inode_id)?;
                let (target_id, phys_blk) = DiskManager::locate_entry_in_dir(
                    &guard.backing,
                    &parent_inode,
                    &stored_name,
                    stored_hash,
                )
                .ok_or_else(|| {
                    DiskManagerError::Io(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "File not found",
                    ))
                })?;

                let target_inode = DiskManager::read_inode_internal(guard, target_id)?;
                let blocks_to_free =
                    DiskManager::collect_inode_blocks(&guard.backing, &target_inode);

                let mut blk_buf =
                    guard
                        .backing
                        .with_block(phys_blk, |s| s.to_vec())
                        .map_err(|_| {
                            DiskManagerError::Io(std::io::Error::other("Directory block not found"))
                        })?;
                remove_entry_in_block_bytes(&mut blk_buf, &stored_name, stored_hash);
                guard
                    .backing
                    .write_all_at(phys_blk * BLOCK_SIZE as u64, &blk_buf)?;

                free_inode_in_place(guard, target_id)?;
                touched_inode_bm = true;
                if !blocks_to_free.is_empty() {
                    free_blocks_in_place(guard, &blocks_to_free)?;
                    touched_data_bm = true;
                }
                guard.inode_cache.remove(target_id);
                guard.dir_cache.remove(target_id);
                touched_dirs.insert(parent_inode_id);
                touched_dirs.insert(target_id);

                parent_inode.modified_at = now;
                DiskManager::write_inode_internal(guard, parent_inode_id, &parent_inode)?;
                if track_ranges {
                    dirty_ranges.push(guard.block_byte_range(phys_blk));
                    dirty_ranges.push(guard.inode_byte_range(parent_inode_id));
                }
            }
            PreparedBatchOp::Rename {
                old_parent_inode_id,
                old_name,
                old_stored_name: old_stored,
                old_stored_hash: old_hash,
                new_parent_inode_id,
                new_name,
                new_stored_name: new_stored,
                new_stored_hash: new_hash,
            } => {
                if old_parent_inode_id == new_parent_inode_id && old_name == new_name {
                    continue;
                }
                let mut old_parent = DiskManager::read_inode_internal(guard, old_parent_inode_id)?;
                let same_parent = old_parent_inode_id == new_parent_inode_id;
                let mut new_parent_opt = if same_parent {
                    None
                } else {
                    Some(DiskManager::read_inode_internal(
                        guard,
                        new_parent_inode_id,
                    )?)
                };

                let (src_id, src_phys_blk) = DiskManager::locate_entry_in_dir(
                    &guard.backing,
                    &old_parent,
                    &old_stored,
                    old_hash,
                )
                .ok_or_else(|| {
                    DiskManagerError::Io(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "File not found",
                    ))
                })?;

                let dst_loc = {
                    let np_ref = new_parent_opt.as_ref().unwrap_or(&old_parent);
                    DiskManager::locate_entry_in_dir(&guard.backing, np_ref, &new_stored, new_hash)
                };
                if let Some((dst_id, _)) = dst_loc
                    && dst_id == src_id
                {
                    continue;
                }

                // Remove old entry from source dir block.
                let mut src_blk_buf = guard
                    .backing
                    .with_block(src_phys_blk, |s| s.to_vec())
                    .map_err(|_| {
                        DiskManagerError::Io(std::io::Error::other("Directory block not found"))
                    })?;
                remove_entry_in_block_bytes(&mut src_blk_buf, &old_stored, old_hash);
                guard
                    .backing
                    .write_all_at(src_phys_blk * BLOCK_SIZE as u64, &src_blk_buf)?;
                if track_ranges {
                    dirty_ranges.push(guard.block_byte_range(src_phys_blk));
                }

                // Remove overwritten target if present.
                if let Some((dst_id, dst_phys_blk)) = dst_loc {
                    let dst_inode = DiskManager::read_inode_internal(guard, dst_id)?;
                    let dst_blocks = DiskManager::collect_inode_blocks(&guard.backing, &dst_inode);
                    let mut dst_blk_buf = guard
                        .backing
                        .with_block(dst_phys_blk, |s| s.to_vec())
                        .map_err(|_| {
                            DiskManagerError::Io(std::io::Error::other("Directory block not found"))
                        })?;
                    remove_entry_in_block_bytes(&mut dst_blk_buf, &new_stored, new_hash);
                    guard
                        .backing
                        .write_all_at(dst_phys_blk * BLOCK_SIZE as u64, &dst_blk_buf)?;
                    if track_ranges {
                        dirty_ranges.push(guard.block_byte_range(dst_phys_blk));
                    }
                    free_inode_in_place(guard, dst_id)?;
                    touched_inode_bm = true;
                    if !dst_blocks.is_empty() {
                        free_blocks_in_place(guard, &dst_blocks)?;
                        touched_data_bm = true;
                    }
                    guard.inode_cache.remove(dst_id);
                    guard.dir_cache.remove(dst_id);
                    touched_dirs.insert(dst_id);
                }

                {
                    let np_mut = new_parent_opt.as_mut().unwrap_or(&mut old_parent);
                    insert_dir_entry_in_place(
                        guard,
                        new_parent_inode_id,
                        np_mut,
                        src_id,
                        &new_stored,
                        new_hash,
                        &mut touched_data_bm,
                        track_ranges.then_some(&mut dirty_ranges),
                    )?;
                }

                old_parent.modified_at = now;
                DiskManager::write_inode_internal(guard, old_parent_inode_id, &old_parent)?;
                if let Some(mut np) = new_parent_opt {
                    np.modified_at = now;
                    DiskManager::write_inode_internal(guard, new_parent_inode_id, &np)?;
                    if track_ranges {
                        dirty_ranges.push(guard.inode_byte_range(new_parent_inode_id));
                    }
                }
                if track_ranges {
                    dirty_ranges.push(guard.inode_byte_range(old_parent_inode_id));
                }
                touched_dirs.insert(old_parent_inode_id);
                touched_dirs.insert(new_parent_inode_id);
            }
        }
    }

    for dir_id in touched_dirs {
        guard.dir_cache.remove(dir_id);
    }

    if track_ranges {
        if touched_inode_bm {
            dirty_ranges.push(guard.inode_bitmap_byte_range());
        }
        if touched_data_bm {
            dirty_ranges.push(guard.data_bitmap_byte_range());
        }
        guard.sync_mutation_ranges(&dirty_ranges)?;
    } else if guard.durability_mode() == DurabilityMode::LegacyWholeMmapAsync {
        guard.sync_mutation_ranges(&[])?;
    }

    Ok(created_inodes)
}

fn allocate_inode_in_place(guard: &mut DiskManagerInner) -> Result<u64, DiskManagerError> {
    let hint = guard.free_inode_hint;
    let usable_inodes = guard.superblock.usable_inode_count();
    let new_inode_id = {
        let mut bitmap_slice = guard
            .inode_bitmap_slice_mut()
            .ok_or_else(|| DiskManagerError::Io(std::io::Error::other("inode bitmap not found")))?;
        let mut allocator = SimpleBlockAllocator::new(&mut bitmap_slice, 0);
        let id = allocator.allocate_bounded(Some(hint), usable_inodes)?;
        bitmap_slice.set_dirty_bits(id as usize, 1);
        id
    };
    guard.free_inode_hint = if new_inode_id + 1 >= usable_inodes {
        0
    } else {
        new_inode_id + 1
    };
    Ok(new_inode_id)
}

fn free_inode_in_place(
    guard: &mut DiskManagerInner,
    inode_id: u64,
) -> Result<(), DiskManagerError> {
    {
        let mut bitmap_slice = guard
            .inode_bitmap_slice_mut()
            .ok_or_else(|| DiskManagerError::Io(std::io::Error::other("inode bitmap not found")))?;
        let mut allocator = SimpleBlockAllocator::new(&mut bitmap_slice, 0);
        allocator.free(inode_id)?;
        bitmap_slice.set_dirty_bits(inode_id as usize, 1);
    }
    if inode_id < guard.free_inode_hint {
        guard.free_inode_hint = inode_id;
    }
    Ok(())
}

fn allocate_single_block_in_place(guard: &mut DiskManagerInner) -> Result<u64, DiskManagerError> {
    let hint = guard.free_block_hint;
    let data_start = guard.superblock.data_block_start;
    let block_count = guard.superblock.block_count;
    let blk = {
        let mut slice = guard
            .data_bitmap_slice_mut()
            .ok_or_else(|| DiskManagerError::Io(std::io::Error::other("data bitmap not found")))?;
        let mut allocator = SimpleBlockAllocator::new(&mut slice, data_start);
        let blk = allocator.allocate_bounded(Some(hint), block_count)?;
        slice.set_dirty_bits((blk - data_start) as usize, 1);
        blk
    };
    guard.free_block_hint = if blk + 1 >= block_count {
        data_start
    } else {
        blk + 1
    };
    Ok(blk)
}

fn free_blocks_in_place(
    guard: &mut DiskManagerInner,
    blocks: &[u64],
) -> Result<(), DiskManagerError> {
    let db_start = guard.superblock.data_block_start;
    let mut min_freed = u64::MAX;
    {
        let mut slice = guard
            .data_bitmap_slice_mut()
            .ok_or_else(|| DiskManagerError::Io(std::io::Error::other("data bitmap not found")))?;
        {
            let mut da = SimpleBlockAllocator::new(&mut slice, db_start);
            for &blk in blocks {
                da.free(blk)?;
                min_freed = min_freed.min(blk);
            }
        }
        for &blk in blocks {
            slice.set_dirty_bits((blk - db_start) as usize, 1);
        }
    }
    if min_freed < guard.free_block_hint {
        guard.free_block_hint = min_freed;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn insert_dir_entry_in_place(
    guard: &mut DiskManagerInner,
    _parent_inode_id: u64,
    parent_inode: &mut Inode,
    child_inode_id: u64,
    stored_name: &str,
    stored_hash: u64,
    touched_data_bm: &mut bool,
    mut dirty_ranges: Option<&mut Vec<(usize, usize)>>,
) -> Result<(), DiskManagerError> {
    let entry = DirectoryEntry {
        inode: child_inode_id,
        hash: stored_hash,
        name: stored_name.to_string(),
    };
    let mut entry_bytes = Vec::with_capacity(20 + entry.name.len());
    {
        let mut cursor = std::io::Cursor::new(&mut entry_bytes);
        entry
            .serialize_into(&mut cursor)
            .map_err(|e| DiskManagerError::Io(std::io::Error::other(e.to_string())))?;
    }
    let needed_space = 20 + entry.name.len();
    let num_blocks = DiskManager::dir_num_blocks(parent_inode);

    let mut slot: Option<(u64, usize)> = None;
    for blk_idx in 0..num_blocks {
        let phys_blk = DiskManager::resolve_logical_block_id(&guard.backing, parent_inode, blk_idx);
        if !guard.is_data_block(phys_blk) {
            continue;
        }
        let insert_off = guard
            .backing
            .with_block(phys_blk, find_insert_offset_in_block)
            .unwrap_or(BLOCK_SIZE);
        if insert_off + needed_space <= BLOCK_SIZE {
            slot = Some((phys_blk, insert_off));
            break;
        }
    }

    if let Some((phys_blk, insert_off)) = slot {
        let mut blk_buf = guard
            .backing
            .with_block(phys_blk, |s| s.to_vec())
            .map_err(|_| {
                DiskManagerError::Io(std::io::Error::other("Directory block not found"))
            })?;
        blk_buf[insert_off..insert_off + entry_bytes.len()].copy_from_slice(&entry_bytes);
        let end = insert_off + entry_bytes.len();
        if end + 18 <= BLOCK_SIZE {
            blk_buf[end..end + 18].fill(0);
        }
        guard
            .backing
            .write_all_at(phys_blk * BLOCK_SIZE as u64, &blk_buf)?;
        if let Some(ranges) = dirty_ranges.as_deref_mut() {
            ranges.push(guard.block_byte_range(phys_blk));
        }
    } else {
        let new_blk_idx = num_blocks;
        let phys_blk = DiskManager::get_or_alloc_block(guard, parent_inode, new_blk_idx, true)?;
        *touched_data_bm = true;
        let mut blk_img = vec![0u8; BLOCK_SIZE];
        blk_img[..entry_bytes.len()].copy_from_slice(&entry_bytes);
        guard
            .backing
            .write_all_at(phys_blk * BLOCK_SIZE as u64, &blk_img)?;
        parent_inode.size = (new_blk_idx + 1) as u64 * BLOCK_SIZE as u64;
        if let Some(ranges) = dirty_ranges {
            ranges.push(guard.block_byte_range(phys_blk));
        }
    }
    Ok(())
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    #[kani::proof]
    #[kani::unwind(8)]
    fn proof_batch_wal_chunking_bounds_and_conservation() {
        let n: usize = kani::any();
        kani::assume((1..=6).contains(&n));

        let mut ops_committed = 0usize;
        let mut chunks_flushed = 0usize;
        let mut cur_est = 0usize;
        let mut cur_ops = 0usize;

        for _ in 0..n {
            // Every real WAL batch op estimates at least 360 bytes (see `estimate_op_wal_bytes`).
            let next_est: usize = kani::any();
            kani::assume((360..=BATCH_WAL_BUDGET * 2).contains(&next_est));
            if should_flush_wal_chunk(cur_est, next_est) {
                assert!(cur_ops > 0);
                if cur_ops > 1 {
                    assert!(cur_est <= BATCH_WAL_BUDGET);
                }
                ops_committed += cur_ops;
                chunks_flushed += 1;
                cur_est = 0;
                cur_ops = 0;
            }
            cur_est = cur_est.saturating_add(next_est);
            cur_ops += 1;
        }

        if cur_ops > 0 {
            if cur_ops > 1 {
                assert!(cur_est <= BATCH_WAL_BUDGET);
            }
            ops_committed += cur_ops;
            chunks_flushed += 1;
        }

        assert_eq!(ops_committed, n);
        assert!(chunks_flushed >= 1 && chunks_flushed <= n);
    }

    #[kani::proof]
    fn proof_batch_wal_byte_estimate_monotone_and_no_overflow() {
        let a: usize = kani::any();
        let b: usize = kani::any();
        kani::assume(a <= b);
        let ea = estimate_standard_create_wal_bytes(a);
        let eb = estimate_standard_create_wal_bytes(b);
        assert!(ea >= 360);
        assert!(ea <= eb);

        let c: usize = kani::any();
        let d: usize = kani::any();
        kani::assume(c <= d);
        let ec = estimate_seekable_create_wal_bytes(c);
        let ed = estimate_seekable_create_wal_bytes(d);
        assert!(ec >= 360);
        assert!(ec <= ed);
    }
}
