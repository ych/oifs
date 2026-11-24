//! In-memory caching structures for `DiskManager`:
//! - [`ChunkReadCache`]: Bounded sub-chunk decompression cache for seekable 64K files
//! - [`DecodedBlobCache`]: Bounded decoded-payload cache for whole-file compressed/encrypted blobs
//! - [`DirIndex`] & [`ShardedDirCache`]: 32-shard lock-striped directory name index
//! - [`BoundedInodeCache`]: 32-shard Second-Chance (Clock) bounded inode cache

use super::*;

/// Bounded sub-chunk decompression cache for seekable 64K files.
///
/// Small positional reads (`read_at` with `< 64 KiB` buffers) otherwise decompress
/// and decrypt the same 64 KiB chunk repeatedly (e.g. 16x per chunk for 4 KiB reads).
type ChunkCacheKey = (u64, usize, u64, usize);

#[derive(Default)]
pub(crate) struct ChunkReadCache {
    entries: VecDeque<(ChunkCacheKey, Arc<[u8]>)>,
}

impl ChunkReadCache {
    const CAPACITY: usize = 8;

    #[inline]
    pub(crate) fn has_chunk(&self, inode_id: u64, chunk_idx: usize) -> bool {
        self.entries
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
        let key = (inode_id, chunk_idx, raw_entry, valid_len);
        let data = self
            .entries
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
        let key = (inode_id, chunk_idx, raw_entry, valid_len);
        let data = self
            .entries
            .iter()
            .rev()
            .find_map(|(k, v)| if *k == key { Some(v) } else { None })?;
        let available = data.len().saturating_sub(in_chunk_offset).min(max_len);
        Some(data[in_chunk_offset..in_chunk_offset + available].to_vec())
    }

    pub(crate) fn insert(
        &mut self,
        inode_id: u64,
        chunk_idx: usize,
        raw_entry: u64,
        valid_len: usize,
        data: Arc<[u8]>,
    ) {
        let key = (inode_id, chunk_idx, raw_entry, valid_len);
        if let Some(pos) = self.entries.iter().position(|(k, _)| *k == key) {
            self.entries.remove(pos);
        }
        if self.entries.len() >= Self::CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back((key, data));
    }

    #[inline]
    pub(crate) fn invalidate_inode(&mut self, inode_id: u64) {
        if !self.entries.is_empty() {
            self.entries.retain(|((id, _, _, _), _)| *id != inode_id);
        }
    }

    #[inline]
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
}

/// Bounded decoded-payload cache for positional `read_at` on non-seekable
/// compressed/encrypted files.
///
/// Eliminates the $O(\text{calls} \times \text{file\_size})$ full-file decode cliff when
/// callers issue sequential or random sub-file `read_at` calls on stream-compressed or
/// whole-file encrypted blobs.
#[derive(Default)]
pub(crate) struct DecodedBlobCache {
    entries: VecDeque<(DecodedBlobKey, Arc<Vec<u8>>)>,
    total_bytes: usize,
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
    pub(crate) const MAX_ENTRIES: usize = 4;
    pub(crate) const MAX_ENTRY_BYTES: usize = 16 * 1024 * 1024;
    pub(crate) const MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;

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
        let key = Self::key_for(inode_id, inode);
        self.entries
            .iter()
            .rev()
            .find_map(|(k, v)| if *k == key { Some(Arc::clone(v)) } else { None })
    }

    pub(crate) fn insert(&mut self, inode_id: u64, inode: &Inode, blob: Arc<Vec<u8>>) {
        let len = blob.len();
        if len > Self::MAX_ENTRY_BYTES {
            return;
        }
        self.invalidate_inode(inode_id);
        while !self.entries.is_empty()
            && (self.entries.len() >= Self::MAX_ENTRIES
                || self.total_bytes.saturating_add(len) > Self::MAX_TOTAL_BYTES)
        {
            if let Some((_, evicted)) = self.entries.pop_front() {
                self.total_bytes = self.total_bytes.saturating_sub(evicted.len());
            }
        }
        self.total_bytes = self.total_bytes.saturating_add(len);
        self.entries
            .push_back((Self::key_for(inode_id, inode), blob));
    }

    #[inline]
    pub(crate) fn invalidate_inode(&mut self, inode_id: u64) {
        if !self.entries.is_empty() {
            self.entries.retain(|(k, v)| {
                if k.inode_id == inode_id {
                    self.total_bytes = self.total_bytes.saturating_sub(v.len());
                    false
                } else {
                    true
                }
            });
        }
    }

    #[inline]
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.total_bytes = 0;
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
pub(crate) struct ShardedDirCache {
    shards: Box<[RwLock<FxHashMap<u64, DirIndex>>]>,
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
            .map(|_| RwLock::new(FxHashMap::default()))
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
    /// Second-chance (Clock) reference bit: set with `Relaxed` atomics under
    /// the shard's shared `.read()` lock so hot directory/file inodes survive cold
    /// sequential scans without requiring a write lock on cache hits.
    referenced: std::sync::atomic::AtomicBool,
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
        entry
            .referenced
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Some(entry.inode)
    }

    fn insert(&mut self, inode_id: u64, inode: Inode) {
        // Re-inserting an existing id refreshes its payload and marks it referenced
        // without queueing a duplicate eviction slot.
        if let Some(existing) = self.map.get_mut(&inode_id) {
            existing.inode = inode;
            existing
                .referenced
                .store(true, std::sync::atomic::Ordering::Relaxed);
        } else {
            self.map.insert(
                inode_id,
                CachedInodeEntry {
                    inode,
                    referenced: std::sync::atomic::AtomicBool::new(false),
                },
            );
            self.order.push_back(inode_id);
        }
        while self.map.len() > self.capacity {
            let Some(candidate) = self.order.pop_front() else {
                break;
            };
            let give_second_chance = self.map.get(&candidate).is_some_and(|e| {
                e.referenced
                    .swap(false, std::sync::atomic::Ordering::Relaxed)
            });
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

/// Sharded, thread-safe bounded inode cache.
///
/// Divides the inode cache into 32 independent shards, each with its own `RwLock`.
/// Concurrent readers and writers on different inodes access completely separate shards
/// with zero lock contention.
///
/// Inode IDs are uniformly distributed across shards using a 64-bit Fibonacci hashing
/// bijection, ensuring consecutive sequential inode IDs never map to the same shard.
pub(crate) struct BoundedInodeCache {
    shards: Box<[RwLock<InodeCacheShard>]>,
    num_shards: usize,
}

impl BoundedInodeCache {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        if capacity <= 16 {
            Self {
                shards: vec![RwLock::new(InodeCacheShard::new(capacity))].into_boxed_slice(),
                num_shards: 1,
            }
        } else {
            let num_shards = NUM_INODE_CACHE_SHARDS;
            let shard_cap = std::cmp::max(1, capacity / num_shards);
            let shards = (0..num_shards)
                .map(|_| RwLock::new(InodeCacheShard::new(shard_cap)))
                .collect::<Vec<_>>()
                .into_boxed_slice();
            Self { shards, num_shards }
        }
    }

    #[inline]
    fn shard_index(&self, inode_id: u64) -> usize {
        if self.num_shards == 1 {
            0
        } else {
            (inode_id.wrapping_mul(0x517cc1b727220a95) as usize) & (self.num_shards - 1)
        }
    }

    pub(crate) fn get(&self, inode_id: u64) -> Option<Inode> {
        let idx = self.shard_index(inode_id);
        self.shards[idx].read().unwrap().get(inode_id)
    }

    pub(crate) fn insert(&self, inode_id: u64, inode: Inode) {
        let idx = self.shard_index(inode_id);
        self.shards[idx].write().unwrap().insert(inode_id, inode);
    }

    pub(crate) fn remove(&self, inode_id: u64) {
        let idx = self.shard_index(inode_id);
        self.shards[idx].write().unwrap().remove(inode_id);
    }

    /// Drop every cached inode.
    ///
    /// Used by a format migration, which rewrites every slot underneath the cache.
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
}
