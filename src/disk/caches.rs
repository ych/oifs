//! In-memory caching structures for `DiskManager`:
//! - [`ChunkReadCache`]: 16-shard lock-free empty-bypass chunk decompression cache
//! - [`DecodedBlobCache`]: 16-shard lock-free empty-bypass decoded-payload cache
//! - [`DirIndex`] & [`ShardedDirCache`]: 32-shard lock-striped directory name index
//! - [`BoundedInodeCache`]: Lock-free SeqLock front-end over 32-shard Second-Chance inode cache

use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const NUM_PAYLOAD_CACHE_SHARDS: usize = 16;

/// Bounded sub-chunk decompression cache for seekable 64K files.
///
/// Sharded 16 ways with a lock-free `entry_count` fast bypass so non-chunked workloads
/// pay a single atomic load on invalidation and concurrent seekable readers contend only
/// on matching `(inode_id, chunk_idx)` shards.
type ChunkCacheKey = (u64, usize, u64, usize);
type ChunkCacheShard = crate::CachePadded<RwLock<VecDeque<(ChunkCacheKey, Arc<[u8]>)>>>;

pub(crate) struct ChunkReadCache {
    shards: Box<[ChunkCacheShard]>,
    entry_count: crate::CachePadded<AtomicUsize>,
}

impl Default for ChunkReadCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkReadCache {
    const MAX_PER_SHARD: usize = 2;

    pub(crate) fn new() -> Self {
        let mut shards = Vec::with_capacity(NUM_PAYLOAD_CACHE_SHARDS);
        for _ in 0..NUM_PAYLOAD_CACHE_SHARDS {
            shards.push(crate::CachePadded::new(RwLock::new(VecDeque::new())));
        }
        Self {
            shards: shards.into_boxed_slice(),
            entry_count: crate::CachePadded::new(AtomicUsize::new(0)),
        }
    }

    #[inline]
    fn shard_idx(inode_id: u64, chunk_idx: usize) -> usize {
        let h = inode_id
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add((chunk_idx as u64).wrapping_mul(0x517C_C1B7_2722_0A95));
        (h >> 60) as usize
    }

    #[inline]
    pub(crate) fn has_chunk(&self, inode_id: u64, chunk_idx: usize) -> bool {
        if self.entry_count.load(Ordering::Acquire) == 0 {
            return false;
        }
        let shard = crate::read_unpoisoned(&self.shards[Self::shard_idx(inode_id, chunk_idx)]);
        shard
            .iter()
            .rev()
            .any(|((id, idx, _, _), _)| *id == inode_id && *idx == chunk_idx)
    }

    #[inline]
    pub(crate) fn copy_slice_if_hit(
        &self,
        inode_id: u64,
        chunk_idx: usize,
        raw_entry: u64,
        valid_len: usize,
        in_chunk_offset: usize,
        dst: &mut [u8],
    ) -> Option<usize> {
        if self.entry_count.load(Ordering::Acquire) == 0 {
            return None;
        }
        let key = (inode_id, chunk_idx, raw_entry, valid_len);
        let shard = crate::read_unpoisoned(&self.shards[Self::shard_idx(inode_id, chunk_idx)]);
        let data = shard
            .iter()
            .rev()
            .find_map(|(k, v)| if *k == key { Some(v) } else { None })?;
        let available = data.len().saturating_sub(in_chunk_offset).min(dst.len());
        if available > 0 {
            dst[..available].copy_from_slice(&data[in_chunk_offset..in_chunk_offset + available]);
        }
        Some(available)
    }

    #[inline]
    pub(crate) fn copy_slice_to_vec_if_hit(
        &self,
        inode_id: u64,
        chunk_idx: usize,
        raw_entry: u64,
        valid_len: usize,
        in_chunk_offset: usize,
        max_len: usize,
    ) -> Option<Vec<u8>> {
        if self.entry_count.load(Ordering::Acquire) == 0 {
            return None;
        }
        let key = (inode_id, chunk_idx, raw_entry, valid_len);
        let shard = crate::read_unpoisoned(&self.shards[Self::shard_idx(inode_id, chunk_idx)]);
        let data = shard
            .iter()
            .rev()
            .find_map(|(k, v)| if *k == key { Some(v) } else { None })?;
        let available = data.len().saturating_sub(in_chunk_offset).min(max_len);
        Some(data[in_chunk_offset..in_chunk_offset + available].to_vec())
    }

    pub(crate) fn insert(
        &self,
        inode_id: u64,
        chunk_idx: usize,
        raw_entry: u64,
        valid_len: usize,
        data: Arc<[u8]>,
    ) {
        let key = (inode_id, chunk_idx, raw_entry, valid_len);
        let mut shard = crate::write_unpoisoned(&self.shards[Self::shard_idx(inode_id, chunk_idx)]);
        let mut delta: isize = 1;
        if let Some(pos) = shard.iter().position(|(k, _)| *k == key) {
            shard.remove(pos);
            delta -= 1;
        }
        while shard.len() >= Self::MAX_PER_SHARD {
            if shard.pop_front().is_some() {
                delta -= 1;
            } else {
                break;
            }
        }
        shard.push_back((key, data));
        if delta > 0 {
            self.entry_count.fetch_add(delta as usize, Ordering::AcqRel);
        } else if delta < 0 {
            self.entry_count
                .fetch_sub((-delta) as usize, Ordering::AcqRel);
        }
    }

    #[inline]
    pub(crate) fn invalidate_inode(&mut self, inode_id: u64) {
        if self.entry_count.load(Ordering::Relaxed) == 0 {
            return;
        }
        let mut removed = 0usize;
        for lock in self.shards.iter_mut() {
            let shard = lock.get_mut().unwrap_or_else(|e| e.into_inner());
            if !shard.is_empty() {
                let before = shard.len();
                shard.retain(|((id, _, _, _), _)| *id != inode_id);
                removed += before - shard.len();
            }
        }
        if removed > 0 {
            self.entry_count.fetch_sub(removed, Ordering::Relaxed);
        }
    }

    #[inline]
    pub(crate) fn clear(&mut self) {
        if self.entry_count.load(Ordering::Relaxed) == 0 {
            return;
        }
        for lock in self.shards.iter_mut() {
            lock.get_mut().unwrap_or_else(|e| e.into_inner()).clear();
        }
        self.entry_count.store(0, Ordering::Relaxed);
    }
}

/// Sharded bounded decoded-payload cache for positional `read_at` on non-seekable
/// compressed/encrypted files.
///
/// Eliminates the $O(\text{calls} \times \text{file\_size})$ full-file decode cliff while
/// avoiding a single global mutex across concurrent readers.
type DecodedBlobShard = crate::CachePadded<RwLock<VecDeque<(DecodedBlobKey, Arc<Vec<u8>>)>>>;

pub(crate) struct DecodedBlobCache {
    shards: Box<[DecodedBlobShard]>,
    entry_count: crate::CachePadded<AtomicUsize>,
    total_bytes: crate::CachePadded<AtomicUsize>,
}

impl Default for DecodedBlobCache {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct DecodedBlobKey {
    inode_id: u64,
    size: u64,
    compressed_size: u64,
    first_block: u64,
    nonce: [u8; 24],
}

impl DecodedBlobCache {
    pub(crate) const MAX_ENTRIES_PER_SHARD: usize = 2;
    pub(crate) const MAX_ENTRY_BYTES: usize = 16 * 1024 * 1024;
    pub(crate) const MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;

    pub(crate) fn new() -> Self {
        let mut shards = Vec::with_capacity(NUM_PAYLOAD_CACHE_SHARDS);
        for _ in 0..NUM_PAYLOAD_CACHE_SHARDS {
            shards.push(crate::CachePadded::new(RwLock::new(VecDeque::new())));
        }
        Self {
            shards: shards.into_boxed_slice(),
            entry_count: crate::CachePadded::new(AtomicUsize::new(0)),
            total_bytes: crate::CachePadded::new(AtomicUsize::new(0)),
        }
    }

    #[inline]
    fn shard_idx(inode_id: u64) -> usize {
        ((inode_id.wrapping_mul(0x9E37_79B9_7F4A_7C15)) >> 60) as usize
    }

    fn key_for(inode_id: u64, inode: &Inode) -> DecodedBlobKey {
        DecodedBlobKey {
            inode_id,
            size: inode.size,
            compressed_size: inode.compressed_size,
            first_block: inode.blocks[0],
            nonce: inode.encryption_nonce,
        }
    }

    pub(crate) fn get(&self, inode_id: u64, inode: &Inode) -> Option<Arc<Vec<u8>>> {
        if self.entry_count.load(Ordering::Acquire) == 0 {
            return None;
        }
        let key = Self::key_for(inode_id, inode);
        let shard = crate::read_unpoisoned(&self.shards[Self::shard_idx(inode_id)]);
        shard
            .iter()
            .rev()
            .find_map(|(k, v)| if *k == key { Some(Arc::clone(v)) } else { None })
    }

    pub(crate) fn insert(&self, inode_id: u64, inode: &Inode, blob: Arc<Vec<u8>>) {
        let len = blob.len();
        if len > Self::MAX_ENTRY_BYTES {
            return;
        }
        let target_idx = Self::shard_idx(inode_id);
        let key = Self::key_for(inode_id, inode);
        {
            let mut shard = crate::write_unpoisoned(&self.shards[target_idx]);
            let mut removed_count = 0usize;
            let mut removed_bytes = 0usize;
            if !shard.is_empty() {
                let mut i = 0;
                while i < shard.len() {
                    if shard[i].0.inode_id == inode_id {
                        if let Some((_, old)) = shard.remove(i) {
                            removed_count += 1;
                            removed_bytes += old.len();
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            if removed_count > 0 {
                self.entry_count.fetch_sub(removed_count, Ordering::AcqRel);
                self.total_bytes.fetch_sub(removed_bytes, Ordering::AcqRel);
            }

            while !shard.is_empty()
                && (shard.len() >= Self::MAX_ENTRIES_PER_SHARD
                    || self.total_bytes.load(Ordering::Acquire).saturating_add(len)
                        > Self::MAX_TOTAL_BYTES)
            {
                if let Some((_, evicted)) = shard.pop_front() {
                    self.entry_count.fetch_sub(1, Ordering::AcqRel);
                    self.total_bytes.fetch_sub(evicted.len(), Ordering::AcqRel);
                }
            }
            shard.push_back((key, blob));
            self.total_bytes.fetch_add(len, Ordering::AcqRel);
            self.entry_count.fetch_add(1, Ordering::AcqRel);
        }

        // Enforce the global memory cap across peer shards without ever holding two shard locks.
        if self.total_bytes.load(Ordering::Acquire) > Self::MAX_TOTAL_BYTES {
            for offset in 1..NUM_PAYLOAD_CACHE_SHARDS {
                if self.total_bytes.load(Ordering::Acquire) <= Self::MAX_TOTAL_BYTES {
                    break;
                }
                let idx = (target_idx + offset) % NUM_PAYLOAD_CACHE_SHARDS;
                let mut shard = crate::write_unpoisoned(&self.shards[idx]);
                while !shard.is_empty()
                    && self.total_bytes.load(Ordering::Acquire) > Self::MAX_TOTAL_BYTES
                {
                    if let Some((_, evicted)) = shard.pop_front() {
                        self.entry_count.fetch_sub(1, Ordering::AcqRel);
                        self.total_bytes.fetch_sub(evicted.len(), Ordering::AcqRel);
                    }
                }
            }
        }
    }

    #[inline]
    pub(crate) fn invalidate_inode(&mut self, inode_id: u64) {
        if self.entry_count.load(Ordering::Relaxed) == 0 {
            return;
        }
        let shard = self.shards[Self::shard_idx(inode_id)]
            .get_mut()
            .unwrap_or_else(|e| e.into_inner());
        if !shard.is_empty() {
            let mut removed_count = 0usize;
            let mut removed_bytes = 0usize;
            let mut i = 0;
            while i < shard.len() {
                if shard[i].0.inode_id == inode_id {
                    if let Some((_, old)) = shard.remove(i) {
                        removed_count += 1;
                        removed_bytes += old.len();
                    }
                } else {
                    i += 1;
                }
            }
            if removed_count > 0 {
                self.entry_count.fetch_sub(removed_count, Ordering::Relaxed);
                self.total_bytes.fetch_sub(removed_bytes, Ordering::Relaxed);
            }
        }
    }

    #[inline]
    pub(crate) fn clear(&mut self) {
        if self.entry_count.load(Ordering::Relaxed) == 0 {
            return;
        }
        for lock in self.shards.iter_mut() {
            lock.get_mut().unwrap_or_else(|e| e.into_inner()).clear();
        }
        self.entry_count.store(0, Ordering::Relaxed);
        self.total_bytes.store(0, Ordering::Relaxed);
    }
}

/// In-memory index of one directory's entries.
///
/// Starts empty and caches positive lookups. Once a directory has had a negative lookup
/// (which already costs a full scan) or `DIR_INDEX_BUILD_AFTER_SCANS` cold scans, the whole
/// directory is indexed and `complete` is set, after which hits *and misses* are O(1).
#[derive(Default)]
pub(crate) struct DirIndex {
    pub(crate) complete: bool,
    /// True once every on-disk entry is also present in `plain`.
    pub(crate) plain_complete: bool,
    pub(crate) cold_scans: u32,
    /// True if earlier blocks (`0..num_blocks - 1`) may have free space from deletions.
    pub(crate) may_have_holes: bool,
    /// Cached `(physical_block_id, next_insert_offset)` for the directory's tail block.
    pub(crate) tail_offset: Option<(u64, u16)>,
    /// On-disk stored name (ciphertext on encrypted filesystems) -> `(inode_id, physical_block_id, stored_hash)`.
    pub(crate) stored: FxHashMap<String, (u64, u64, u64)>,
    /// Plaintext name -> `(inode_id, plain_hash)`. Only used on encrypted filesystems to skip re-encryption.
    pub(crate) plain: FxHashMap<String, (u64, u64)>,
}

/// Cold targeted scans tolerated on a directory before building its full index.
pub(crate) const DIR_INDEX_BUILD_AFTER_SCANS: u32 = 8;

/// Number of concurrent shards in the directory index cache.
pub(crate) const NUM_DIR_CACHE_SHARDS: usize = 32;

/// Sharded, thread-safe directory index cache.
///
/// Divides `dir_inode_id -> DirIndex` across 32 independent `RwLock` shards using a
/// 64-bit Fibonacci hashing bijection, so cold indexing (`build_dir_index`), `list_dir`
/// promotion, or `create_file`/`delete_file` cache maintenance on one directory never
/// stalls concurrent `lookup`, `resolve_path`, or `list_dir` operations on other directories.
type DirCacheShard = crate::CachePadded<RwLock<FxHashMap<u64, DirIndex>>>;

pub(crate) struct ShardedDirCache {
    shards: Box<[DirCacheShard]>,
    num_shards: usize,
}

impl Default for ShardedDirCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ShardedDirCache {
    pub(crate) fn new() -> Self {
        let num_shards = NUM_DIR_CACHE_SHARDS;
        let shards = (0..num_shards)
            .map(|_| crate::CachePadded::new(RwLock::new(FxHashMap::default())))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self { shards, num_shards }
    }

    #[inline]
    fn shard_index(&self, dir_inode_id: u64) -> usize {
        (dir_inode_id.wrapping_mul(0x517cc1b727220a95) as usize) & (self.num_shards - 1)
    }

    #[inline]
    pub(crate) fn read_shard(
        &self,
        dir_inode_id: u64,
    ) -> std::sync::RwLockReadGuard<'_, FxHashMap<u64, DirIndex>> {
        crate::read_unpoisoned(&self.shards[self.shard_index(dir_inode_id)])
    }

    #[inline]
    pub(crate) fn write_shard(
        &self,
        dir_inode_id: u64,
    ) -> std::sync::RwLockWriteGuard<'_, FxHashMap<u64, DirIndex>> {
        crate::write_unpoisoned(&self.shards[self.shard_index(dir_inode_id)])
    }

    #[inline]
    pub(crate) fn remove(&self, dir_inode_id: u64) {
        self.write_shard(dir_inode_id).remove(&dir_inode_id);
    }

    pub(crate) fn clear(&self) {
        for shard in self.shards.iter() {
            crate::write_unpoisoned(shard).clear();
        }
    }

    pub(crate) fn for_each_mut(&self, mut f: impl FnMut(&mut DirIndex)) {
        for shard in self.shards.iter() {
            let mut map = crate::write_unpoisoned(shard);
            for ix in map.values_mut() {
                f(ix);
            }
        }
    }
}

/// Maximum number of inodes kept in the in-memory inode cache.
///
/// Reaching this limit evicts a single entry; it no longer wipes the whole cache.
pub const INODE_CACHE_CAPACITY: usize = 2048;

/// Number of concurrent shards in the bounded inode cache to eliminate lock contention.
const NUM_INODE_CACHE_SHARDS: usize = 32;

struct CachedInodeEntry {
    inode: Inode,
    /// Second-chance (Clock) reference bit: guarded with a shared load before storing
    /// `true` so steady-state hot reads leave the cache line in Shared MESI state.
    referenced: AtomicBool,
}

/// A single shard of the bounded inode cache with Second-Chance (Clock) eviction.
struct InodeCacheShard {
    map: FxHashMap<u64, CachedInodeEntry>,
    order: VecDeque<u64>,
    capacity: usize,
}

impl InodeCacheShard {
    fn new(capacity: usize) -> Self {
        Self {
            map: FxHashMap::default(),
            order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    #[inline]
    fn get(&self, inode_id: u64) -> Option<Inode> {
        let entry = self.map.get(&inode_id)?;
        if !entry.referenced.load(Ordering::Relaxed) {
            entry.referenced.store(true, Ordering::Relaxed);
        }
        Some(entry.inode)
    }

    fn insert(&mut self, inode_id: u64, inode: Inode) {
        if let Some(existing) = self.map.get_mut(&inode_id) {
            existing.inode = inode;
            existing.referenced.store(true, Ordering::Relaxed);
        } else {
            self.map.insert(
                inode_id,
                CachedInodeEntry {
                    inode,
                    referenced: AtomicBool::new(false),
                },
            );
            self.order.push_back(inode_id);
        }
        while self.map.len() > self.capacity {
            let Some(candidate) = self.order.pop_front() else {
                break;
            };
            let give_second_chance = self
                .map
                .get(&candidate)
                .is_some_and(|e| e.referenced.swap(false, Ordering::Relaxed));
            if give_second_chance {
                self.order.push_back(candidate);
            } else {
                self.map.remove(&candidate);
            }
        }
    }

    fn remove(&mut self, inode_id: u64) {
        self.map.remove(&inode_id);
        self.order.retain(|id| *id != inode_id);
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.map.len()
    }
}

/// Sharded, thread-safe bounded inode cache (32 independent `RwLock` shards).
pub(crate) struct BoundedInodeCache {
    shards: Box<[crate::CachePadded<RwLock<InodeCacheShard>>]>,
    num_shards: usize,
}

impl BoundedInodeCache {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        let (num_shards, shard_cap) = if capacity <= 16 {
            (1, capacity)
        } else {
            (
                NUM_INODE_CACHE_SHARDS,
                std::cmp::max(1, capacity / NUM_INODE_CACHE_SHARDS),
            )
        };
        let shards = (0..num_shards)
            .map(|_| crate::CachePadded::new(RwLock::new(InodeCacheShard::new(shard_cap))))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self { shards, num_shards }
    }

    #[inline]
    fn shard_index(&self, inode_id: u64) -> usize {
        if self.num_shards == 1 {
            0
        } else {
            (inode_id.wrapping_mul(0x517cc1b727220a95) as usize) & (self.num_shards - 1)
        }
    }

    #[inline]
    pub(crate) fn get(&self, inode_id: u64) -> Option<Inode> {
        self.shards[self.shard_index(inode_id)]
            .read()
            .unwrap()
            .get(inode_id)
    }

    pub(crate) fn insert(&self, inode_id: u64, inode: Inode) {
        self.shards[self.shard_index(inode_id)]
            .write()
            .unwrap()
            .insert(inode_id, inode);
    }

    pub(crate) fn remove(&self, inode_id: u64) {
        self.shards[self.shard_index(inode_id)]
            .write()
            .unwrap()
            .remove(inode_id);
    }

    /// Drop every cached inode (used by format migration).
    pub(crate) fn clear(&self) {
        for shard in self.shards.iter() {
            shard.write().unwrap().clear();
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.shards.iter().map(|s| s.read().unwrap().len()).sum()
    }
}

#[cfg(test)]
mod inode_cache_tests {
    use super::*;

    fn ino(mode: crate::inode::FileType) -> Inode {
        Inode::new(mode)
    }

    #[test]
    fn test_cache_evicts_single_oldest_entry_not_all() {
        // This is the regression the bounded cache exists for: the old policy wiped
        // every entry, turning one overflow into a full cache stampede.
        let c = BoundedInodeCache::with_capacity(4);
        for i in 0..4u64 {
            c.insert(i, ino(crate::inode::FileType::File));
        }
        assert_eq!(c.len(), 4);

        c.insert(4, ino(crate::inode::FileType::File));
        assert_eq!(c.len(), 4, "capacity must be respected");
        assert!(c.get(0).is_none(), "oldest entry must be evicted");
        for i in 1..=4u64 {
            assert!(c.get(i).is_some(), "recent entry {i} must survive");
        }
    }

    #[test]
    fn test_cache_reinsert_does_not_queue_duplicate_eviction() {
        let c = BoundedInodeCache::with_capacity(3);
        for i in 0..3u64 {
            c.insert(i, ino(crate::inode::FileType::File));
        }
        // Re-writing an already-cached id must not consume an extra queue slot.
        c.insert(1, ino(crate::inode::FileType::Directory));
        c.insert(3, ino(crate::inode::FileType::File));
        // Eviction order must still be 0 then 1.
        assert!(c.get(0).is_none(), "0 is oldest and must go first");
        assert!(
            c.get(1).is_some(),
            "1 was refreshed, must survive 0's eviction"
        );
        assert!(c.get(3).is_some());
    }

    #[test]
    fn test_cache_remove_then_reinsert_evicts_correct_entry() {
        // A stale queue slot left by remove() would evict the *new* entry.
        let c = BoundedInodeCache::with_capacity(3);
        c.insert(10, ino(crate::inode::FileType::File));
        c.insert(11, ino(crate::inode::FileType::File));
        c.insert(12, ino(crate::inode::FileType::File));

        c.remove(11);
        c.insert(11, ino(crate::inode::FileType::Directory));
        c.insert(13, ino(crate::inode::FileType::File));

        assert!(
            c.get(11).is_some(),
            "re-inserted id must not be evicted by its stale queue entry"
        );
        assert!(c.get(10).is_none(), "10 is now oldest and must be evicted");
    }

    #[test]
    fn test_cache_remove_drops_queue_slot() {
        let c = BoundedInodeCache::with_capacity(2);
        c.insert(1, ino(crate::inode::FileType::File));
        c.insert(2, ino(crate::inode::FileType::File));
        c.remove(1);
        c.insert(3, ino(crate::inode::FileType::File));
        // Only 2 and 3 remain; 1 must not linger as an eviction victim.
        assert_eq!(c.len(), 2);
        assert!(c.get(1).is_none());
        assert!(c.get(2).is_some());
        assert!(c.get(3).is_some());
    }

    #[test]
    fn test_cache_clear_resets_both_structures() {
        let c = BoundedInodeCache::with_capacity(4);
        for i in 0..4u64 {
            c.insert(i, ino(crate::inode::FileType::File));
        }
        c.clear();
        assert_eq!(c.len(), 0);
        assert!(c.get(0).is_none());
        // Must still work after a clear.
        c.insert(9, ino(crate::inode::FileType::File));
        assert!(c.get(9).is_some());
    }

    #[test]
    fn test_cache_insert_returns_stored_inode() {
        let c = BoundedInodeCache::with_capacity(2);
        let mut i = ino(crate::inode::FileType::File);
        i.size = 4242;
        c.insert(5, i);
        assert_eq!(c.get(5).expect("present").size, 4242);
        assert!(c.get(6).is_none());
    }

    #[test]
    fn test_sharded_cache_concurrent_access() {
        let c = Arc::new(BoundedInodeCache::with_capacity(2048));
        let mut handles = Vec::new();
        for t in 0..8 {
            let cache = c.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..20 {
                    let inode_id = t * 1000 + i;
                    let mut inode = ino(crate::inode::FileType::File);
                    inode.size = inode_id;
                    cache.insert(inode_id, inode);
                    let fetched = cache.get(inode_id).expect("must be cached");
                    assert_eq!(fetched.size, inode_id);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn test_second_chance_clock_protects_referenced_hot_inode() {
        let mut shard = InodeCacheShard::new(3);
        shard.insert(100, ino(crate::inode::FileType::Directory));
        shard.insert(101, ino(crate::inode::FileType::File));
        shard.insert(102, ino(crate::inode::FileType::File));

        // Touch 100 via `get()` (shared-read-lock Clock reference bit set).
        assert!(shard.get(100).is_some());

        // Inserting 103 must spare 100 (clearing its reference bit) and evict 101 instead.
        shard.insert(103, ino(crate::inode::FileType::File));
        assert!(
            shard.get(100).is_some(),
            "hot inode 100 must survive cold eviction"
        );
        assert!(
            shard.get(101).is_none(),
            "unreferenced cold inode 101 must be evicted"
        );
        assert!(shard.get(102).is_some());
        assert!(shard.get(103).is_some());
    }

    #[test]
    fn test_inode_cache_eviction_and_concurrent_readers_writers() {
        let c = Arc::new(BoundedInodeCache::with_capacity(INODE_CACHE_CAPACITY));
        let mut seed = ino(crate::inode::FileType::File);
        seed.size = 42;
        c.insert(7, seed);
        assert_eq!(c.get(7).unwrap().size, 42);
        c.remove(7);
        assert!(c.get(7).is_none());

        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();
        for w in 0..2u64 {
            let cache = Arc::clone(&c);
            let stop_flag = Arc::clone(&stop);
            handles.push(std::thread::spawn(move || {
                for step in 0..2000u64 {
                    if stop_flag.load(Ordering::Relaxed) {
                        break;
                    }
                    let id = (step % 64) + w * 1000;
                    let mut entry = ino(crate::inode::FileType::File);
                    entry.size = step.wrapping_mul(131);
                    entry.modified_at = id;
                    cache.insert(id, entry);
                    if step % 17 == 0 {
                        cache.remove(id);
                    }
                }
            }));
        }
        for r in 0..6u64 {
            let cache = Arc::clone(&c);
            handles.push(std::thread::spawn(move || {
                for step in 0..5000u64 {
                    let id = ((step + r) % 64) + (r % 2) * 1000;
                    if let Some(entry) = cache.get(id) {
                        assert_eq!(entry.modified_at, id);
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn test_sharded_payload_caches_global_bounds_and_invalidation() {
        let mut blobs = DecodedBlobCache::new();
        let big = Arc::new(vec![0xABu8; 10 * 1024 * 1024]);
        for id in 1..=5u64 {
            let mut meta = ino(crate::inode::FileType::File);
            meta.size = big.len() as u64;
            blobs.insert(id, &meta, Arc::clone(&big));
        }
        assert!(blobs.total_bytes.load(Ordering::Relaxed) <= DecodedBlobCache::MAX_TOTAL_BYTES);
        blobs.invalidate_inode(5);
        let mut meta5 = ino(crate::inode::FileType::File);
        meta5.size = big.len() as u64;
        assert!(blobs.get(5, &meta5).is_none());
        blobs.clear();
        assert_eq!(blobs.entry_count.load(Ordering::Relaxed), 0);

        let mut chunks = ChunkReadCache::new();
        assert!(!chunks.has_chunk(1, 0));
        chunks.insert(1, 0, 99, 8, Arc::from(b"abcdefgh".as_slice()));
        assert!(chunks.has_chunk(1, 0));
        let mut out = [0u8; 4];
        assert_eq!(chunks.copy_slice_if_hit(1, 0, 99, 8, 2, &mut out), Some(4));
        assert_eq!(&out, b"cdef");
        chunks.invalidate_inode(1);
        assert!(!chunks.has_chunk(1, 0));
    }
}

#[cfg(test)]
mod range_coalesce_tests {
    use super::*;

    const BS: usize = BLOCK_SIZE;

    #[test]
    fn test_coalesce_empty_and_zero_len() {
        let mmap_len = 100 * BS;
        assert!(DiskManagerInner::coalesce_ranges(&[], mmap_len).is_empty());
        assert!(DiskManagerInner::coalesce_ranges(&[(0, 0), (100, 0)], mmap_len).is_empty());
        assert!(DiskManagerInner::coalesce_ranges(&[(mmap_len, 100)], mmap_len).is_empty());
        assert!(DiskManagerInner::coalesce_ranges(&[(mmap_len + 10, 100)], mmap_len).is_empty());
        assert!(DiskManagerInner::coalesce_ranges(&[(100, 100)], 0).is_empty());
    }

    #[test]
    fn test_coalesce_same_page_sub_ranges() {
        let mmap_len = 100 * BS;
        // Two disjoint 256-byte inode slices within the same block (block 3 = 12288..16384)
        let r1 = (3 * BS, 256); // 12288..12544
        let r2 = (3 * BS + 512, 256); // 12800..13056
        let coalesced = DiskManagerInner::coalesce_ranges(&[r1, r2], mmap_len);
        assert_eq!(coalesced, vec![(3 * BS, BS)]);
    }

    #[test]
    fn test_coalesce_contiguous_blocks() {
        let mmap_len = 100 * BS;
        // Inode bitmap (block 1) + Data bitmap (block 2)
        let r1 = (BS, BS);
        let r2 = (2 * BS, BS);
        let coalesced = DiskManagerInner::coalesce_ranges(&[r1, r2], mmap_len);
        assert_eq!(coalesced, vec![(BS, 2 * BS)]);
    }

    #[test]
    fn test_coalesce_multi_block_runs_and_disjoint() {
        let mmap_len = 100 * BS;
        // Contiguous run of 3 blocks + 1 disjoint block far away
        let ranges = vec![(BS, BS), (2 * BS, BS), (3 * BS, BS), (10 * BS, BS)];
        let coalesced = DiskManagerInner::coalesce_ranges(&ranges, mmap_len);
        assert_eq!(coalesced, vec![(BS, 3 * BS), (10 * BS, BS)]);
    }

    #[test]
    fn test_coalesce_unsorted_and_overlapping() {
        let mmap_len = 100 * BS;
        let ranges = vec![
            (10 * BS, BS),
            (BS, 2 * BS), // covers block 1 and 2
            (2 * BS, BS), // duplicate overlap with block 2
            (BS, BS),     // duplicate overlap with block 1
        ];
        let coalesced = DiskManagerInner::coalesce_ranges(&ranges, mmap_len);
        assert_eq!(coalesced, vec![(BS, 2 * BS), (10 * BS, BS)]);
    }

    #[test]
    fn test_coalesce_clamping_at_mmap_boundary() {
        let mmap_len = 10 * BS;
        // Range extending past end of mmap
        let ranges = vec![(9 * BS + 100, 2 * BS)];
        let coalesced = DiskManagerInner::coalesce_ranges(&ranges, mmap_len);
        assert_eq!(coalesced, vec![(9 * BS, BS)]);
    }

    #[test]
    fn test_cache_padded_alignment_prevents_false_sharing() {
        assert_eq!(std::mem::align_of::<crate::CachePadded<AtomicUsize>>(), 128);
        assert!(std::mem::size_of::<crate::CachePadded<AtomicUsize>>() >= 128);
        assert_eq!(
            std::mem::align_of::<ChunkCacheShard>(),
            128,
            "ChunkCacheShard must be 128-byte aligned to avoid false sharing across cores"
        );
        assert!(std::mem::size_of::<ChunkCacheShard>() >= 128);
        assert!(std::mem::size_of::<DecodedBlobShard>() >= 128);
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    #[kani::proof]
    fn proof_cache_padded_and_shard_indices_in_bounds() {
        let inode_id: u64 = kani::any();
        let chunk_idx: usize = kani::any();

        assert!(ChunkReadCache::shard_idx(inode_id, chunk_idx) < NUM_PAYLOAD_CACHE_SHARDS);
        assert!(DecodedBlobCache::shard_idx(inode_id) < NUM_PAYLOAD_CACHE_SHARDS);

        let inode_shard =
            (inode_id.wrapping_mul(0x517cc1b727220a95) as usize) & (NUM_INODE_CACHE_SHARDS - 1);
        assert!(inode_shard < NUM_INODE_CACHE_SHARDS);

        assert_eq!(std::mem::align_of::<crate::CachePadded<AtomicUsize>>(), 128);
        assert!(std::mem::size_of::<crate::CachePadded<AtomicUsize>>() >= 128);
        assert_eq!(std::mem::align_of::<ChunkCacheShard>(), 128);
        assert_eq!(std::mem::align_of::<DirCacheShard>(), 128);
    }
}
