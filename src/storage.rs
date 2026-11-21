//! Pluggable image storage backing for [`crate::DiskManager`].
//!
//! OIFS supports two image storage backends over the exact same on-disk format:
//!
//! - [`StorageMode::Mmap`] (default): maps the entire `.img` container with `memmap2::MmapMut`
//!   for zero-copy memory access (`40–58 GB/s` on resident pages).
//! - [`StorageMode::FileIo`]: 100% `mmap`-free positional `pread(2)` / `pwrite(2)` I/O backed
//!   by in-memory write-through bitmaps, an in-memory WAL mirror (when journaled), and a
//!   bounded 16-shard 4 KiB metadata block cache. Eliminates `SIGBUS` crashes and `unsafe`
//!   memory-mapping hazards while keeping all I/O strictly in Safe Rust.

use crate::BLOCK_SIZE;
use crate::FxHashMap;
use crate::superblock::SuperBlock;
use memmap2::{MmapMut, MmapOptions};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

/// Environment variable selecting the default storage mode (`mmap` or `pread`/`file`).
pub const STORAGE_MODE_ENV: &str = "OIFS_STORAGE_MODE";

/// Environment variable selecting the default [`StorageMode::FileIo`] user-space data page
/// cache budget in MiB (`0` disables caching; defaults to 32 MiB).
pub const FILE_IO_CACHE_MB_ENV: &str = "OIFS_FILE_IO_CACHE_MB";

/// Default user-space data page cache capacity for [`StorageMode::FileIo`] (32 MiB).
pub const DEFAULT_FILE_IO_CACHE_BYTES: usize = 32 * 1024 * 1024;

/// Maximum user-space data page cache capacity for [`StorageMode::FileIo`] (4 GiB).
pub const MAX_FILE_IO_CACHE_BYTES: usize = 4 * 1024 * 1024 * 1024;

/// Maximum read target size eligible for caching in [`DataPageCache`]. Larger streaming
/// reads bypass the cache to preserve sequential throughput and prevent scan pollution.
pub(crate) const MAX_CACHED_EXTENT: usize = 64 * 1024;

/// Process-wide override for the default storage mode (0 = unset, 1 = Mmap, 2 = FileIo).
static DEFAULT_STORAGE_MODE_OVERRIDE: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(0);

/// Sets a process-wide default [`StorageMode`] used by `DiskManager::open*` constructors
/// that do not specify an explicit mode (takes precedence over [`STORAGE_MODE_ENV`]).
pub fn set_default_storage_mode(mode: StorageMode) {
    let encoded = match mode {
        StorageMode::Mmap => 1,
        StorageMode::FileIo => 2,
    };
    DEFAULT_STORAGE_MODE_OVERRIDE.store(encoded, std::sync::atomic::Ordering::Relaxed);
}

/// Resolves the effective default [`StorageMode`] from the process override,
/// [`STORAGE_MODE_ENV`], or [`StorageMode::Mmap`].
#[inline]
pub fn default_storage_mode() -> StorageMode {
    match DEFAULT_STORAGE_MODE_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed) {
        1 => StorageMode::Mmap,
        2 => StorageMode::FileIo,
        _ => StorageMode::from_env().unwrap_or(StorageMode::Mmap),
    }
}

/// Image storage backend mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[repr(u8)]
pub enum StorageMode {
    /// Memory-mapped container (`memmap2::MmapMut`, default).
    #[default]
    Mmap = 0,
    /// Pure positional `pread`/`pwrite` I/O without calling `mmap()`.
    FileIo = 1,
}

impl StorageMode {
    /// Parses a storage mode name (`mmap`, `pread`, `file`, `file_io`, `no-mmap`).
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "mmap" => Some(Self::Mmap),
            "pread" | "pwrite" | "file" | "file_io" | "fileio" | "no-mmap" | "nommap" => {
                Some(Self::FileIo)
            }
            _ => None,
        }
    }

    /// Reads [`STORAGE_MODE_ENV`]. Unset or unrecognized values return `None`.
    pub fn from_env() -> Option<Self> {
        std::env::var(STORAGE_MODE_ENV)
            .ok()
            .and_then(|v| Self::parse(&v))
    }
}

/// Static zero block used to zero freshly allocated blocks via `pwrite` without heap allocation.
static ZERO_BLOCK: [u8; BLOCK_SIZE] = [0u8; BLOCK_SIZE];

#[cfg(unix)]
#[inline]
pub(crate) fn file_read_exact_at(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

#[cfg(unix)]
#[inline]
pub(crate) fn file_write_all_at(file: &File, offset: u64, buf: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buf, offset)
}

#[cfg(windows)]
pub(crate) fn file_read_exact_at(
    file: &File,
    mut offset: u64,
    mut buf: &mut [u8],
) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = file.seek_read(buf, offset)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "unexpected EOF during positional read",
            ));
        }
        offset += n as u64;
        buf = &mut buf[n..];
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn file_write_all_at(file: &File, mut offset: u64, mut buf: &[u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = file.seek_write(buf, offset)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "failed to write whole buffer during positional write",
            ));
        }
        offset += n as u64;
        buf = &buf[n..];
    }
    Ok(())
}

/// 16-shard bounded 4 KiB metadata block cache (max 1,024 blocks = 4 MiB) for [`FileBacking`].
///
/// Caches indirect pointer blocks and directory blocks accessed via `with_block`, allowing
/// concurrent readers to resolve block pointers and scan directories without repeated `pread`
/// syscalls or cross-thread lock contention.
///
/// Entries are stored behind `Arc<[u8; BLOCK_SIZE]>` so `get()` releases its shard read lock
/// before returning — making nested `with_block` calls (such as double-indirect traversal)
/// strictly deadlock-free even when two blocks hash to the same shard.
struct MetadataBlockCache {
    shards: [RwLock<FxHashMap<u64, Arc<[u8; BLOCK_SIZE]>>>; Self::NUM_SHARDS],
}

impl MetadataBlockCache {
    const NUM_SHARDS: usize = 16;
    const MAX_PER_SHARD: usize = 64; // 16 * 64 * 4 KiB = 4 MiB total cap

    fn new() -> Self {
        Self {
            shards: std::array::from_fn(|_| RwLock::new(FxHashMap::default())),
        }
    }

    #[inline]
    fn shard_idx(block_id: u64) -> usize {
        // Fibonacci hash mix for uniform shard distribution
        ((block_id.wrapping_mul(0x9E37_79B9_7F4A_7C15)) >> 60) as usize
    }

    #[inline]
    fn get(&self, block_id: u64) -> Option<Arc<[u8; BLOCK_SIZE]>> {
        let shard = self.shards[Self::shard_idx(block_id)]
            .read()
            .unwrap_or_else(|e| e.into_inner());
        shard.get(&block_id).cloned()
    }

    /// Reads an 8-byte little-endian `u64` directly under the leaf shard read lock without
    /// cloning the `Arc`, eliminating atomic refcount contention on hot indirect pointer blocks.
    #[inline]
    fn read_u64_le(&self, block_id: u64, off: usize) -> Option<u64> {
        let shard = self.shards[Self::shard_idx(block_id)]
            .read()
            .unwrap_or_else(|e| e.into_inner());
        shard
            .get(&block_id)
            .map(|b| u64::from_le_bytes(b[off..off + 8].try_into().unwrap()))
    }

    fn insert(&self, block_id: u64, block: Arc<[u8; BLOCK_SIZE]>) {
        let mut shard = self.shards[Self::shard_idx(block_id)]
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if shard.len() >= Self::MAX_PER_SHARD && !shard.contains_key(&block_id) {
            // Drop one arbitrary entry rather than wiping the entire shard.
            if let Some(&evict_key) = shard.keys().next() {
                shard.remove(&evict_key);
            }
        }
        shard.insert(block_id, block);
    }

    fn update_if_present(&self, block_id: u64, offset_in_block: usize, data: &[u8]) {
        let mut shard = self.shards[Self::shard_idx(block_id)]
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(arc_blk) = shard.get_mut(&block_id) {
            let end = (offset_in_block + data.len()).min(BLOCK_SIZE);
            if offset_in_block < end {
                let blk = Arc::make_mut(arc_blk);
                blk[offset_in_block..end].copy_from_slice(&data[..end - offset_in_block]);
            }
        }
    }

    fn invalidate(&self, block_id: u64) {
        let mut shard = self.shards[Self::shard_idx(block_id)]
            .write()
            .unwrap_or_else(|e| e.into_inner());
        shard.remove(&block_id);
    }

    fn invalidate_block_range(&self, first_blk: u64, last_blk: u64) {
        for lock in &self.shards {
            let mut shard = lock.write().unwrap_or_else(|e| e.into_inner());
            if !shard.is_empty() {
                shard.retain(|&k, _| k < first_blk || k > last_blk);
            }
        }
    }

    fn clear(&self) {
        for s in &self.shards {
            s.write().unwrap_or_else(|e| e.into_inner()).clear();
        }
    }
}

struct DataCacheEntry {
    data: Box<[u8; BLOCK_SIZE]>,
    referenced: AtomicBool,
}

struct DataCacheShard {
    map: FxHashMap<u64, DataCacheEntry>,
    ring: Vec<u64>,
    hand: usize,
}

impl DataCacheShard {
    fn new() -> Self {
        Self {
            map: FxHashMap::default(),
            ring: Vec::new(),
            hand: 0,
        }
    }
}

/// 64-shard bounded Second-Chance Clock user-space 4 KiB data page cache for [`FileBacking`].
///
/// Serves hot random and small-file reads straight from RAM without `pread` syscalls while
/// staying scan-resistant (large streaming extents `> MAX_CACHED_EXTENT` bypass the cache)
/// and strictly coherent on every write, zero, copy, and WAL replay.
struct DataPageCache {
    shards: [RwLock<DataCacheShard>; Self::NUM_SHARDS],
    /// Maximum cached blocks per shard (`0` disables the cache).
    max_per_shard: AtomicUsize,
    /// Total currently cached blocks across all shards; allows `O(1)` lock-free skip on writes
    /// when the cache is empty.
    count: AtomicUsize,
}

impl DataPageCache {
    const NUM_SHARDS: usize = 64;
    /// Extents strictly larger than 64 KiB bypass the data page cache to preserve sequential
    /// streaming throughput and prevent large scans from evicting hot random-access pages.
    const MAX_CACHED_EXTENT: usize = MAX_CACHED_EXTENT;

    fn default_capacity_bytes() -> usize {
        if let Ok(val) = std::env::var(FILE_IO_CACHE_MB_ENV)
            && let Ok(mb) = val.trim().parse::<usize>()
        {
            return mb.saturating_mul(1024 * 1024).min(MAX_FILE_IO_CACHE_BYTES);
        }
        DEFAULT_FILE_IO_CACHE_BYTES
    }

    fn blocks_per_shard_for_bytes(bytes: usize) -> usize {
        let clamped = bytes.min(MAX_FILE_IO_CACHE_BYTES);
        if clamped == 0 {
            return 0;
        }
        let total_blocks = clamped / BLOCK_SIZE;
        if total_blocks == 0 {
            0
        } else {
            total_blocks.div_ceil(Self::NUM_SHARDS)
        }
    }

    fn new(capacity_bytes: usize) -> Self {
        Self {
            shards: std::array::from_fn(|_| RwLock::new(DataCacheShard::new())),
            max_per_shard: AtomicUsize::new(Self::blocks_per_shard_for_bytes(capacity_bytes)),
            count: AtomicUsize::new(0),
        }
    }

    #[inline]
    fn shard_idx(block_id: u64) -> usize {
        ((block_id.wrapping_mul(0x9E37_79B9_7F4A_7C15)) >> 58) as usize
    }

    #[inline]
    fn is_enabled(&self) -> bool {
        self.max_per_shard.load(Ordering::Relaxed) > 0
    }

    #[inline]
    fn capacity_bytes(&self) -> usize {
        self.max_per_shard
            .load(Ordering::Relaxed)
            .saturating_mul(Self::NUM_SHARDS)
            .saturating_mul(BLOCK_SIZE)
    }

    #[inline]
    fn used_bytes(&self) -> usize {
        self.count
            .load(Ordering::Relaxed)
            .saturating_mul(BLOCK_SIZE)
    }

    fn set_capacity_bytes(&self, bytes: usize) {
        let new_limit = Self::blocks_per_shard_for_bytes(bytes);
        self.max_per_shard.store(new_limit, Ordering::Relaxed);
        if new_limit == 0 {
            self.clear();
            return;
        }
        if self.count.load(Ordering::Relaxed) == 0 {
            return;
        }
        let mut removed = 0usize;
        for lock in &self.shards {
            let mut shard = lock.write().unwrap_or_else(|e| e.into_inner());
            while shard.map.len() > new_limit {
                if let Some(evicted) = Self::evict_one_locked(&mut shard) {
                    let _ = evicted;
                    removed += 1;
                } else {
                    break;
                }
            }
        }
        if removed > 0 {
            self.count.fetch_sub(removed, Ordering::Relaxed);
        }
    }

    fn evict_one_locked(shard: &mut DataCacheShard) -> Option<u64> {
        if shard.map.is_empty() {
            shard.ring.clear();
            shard.hand = 0;
            return None;
        }
        // Compact ring if stale keys accumulated after invalidations.
        if shard.ring.len() > shard.map.len().saturating_mul(2).max(16) {
            shard.ring.retain(|k| shard.map.contains_key(k));
            shard.hand = 0;
        }
        let max_steps = shard.ring.len().saturating_mul(2).saturating_add(1);
        for _ in 0..max_steps {
            if shard.ring.is_empty() {
                break;
            }
            if shard.hand >= shard.ring.len() {
                shard.hand = 0;
            }
            let key = shard.ring[shard.hand];
            let evict_now = match shard.map.get(&key) {
                Some(entry) => !entry.referenced.swap(false, Ordering::Relaxed),
                None => {
                    shard.ring.swap_remove(shard.hand);
                    continue;
                }
            };
            if evict_now {
                shard.ring.swap_remove(shard.hand);
                shard.map.remove(&key);
                return Some(key);
            }
            shard.hand += 1;
        }
        // Fallback if ring somehow emptied while map non-empty.
        if let Some(&key) = shard.map.keys().next() {
            shard.map.remove(&key);
            return Some(key);
        }
        None
    }

    /// Attempts to copy `[in_blk_off..in_blk_off + dst.len()]` from a cached block.
    /// Returns `true` on hit (under a shared shard read lock with lock-free clock bit update).
    #[inline]
    fn try_copy_into(&self, block_id: u64, in_blk_off: usize, dst: &mut [u8]) -> bool {
        let shard = self.shards[Self::shard_idx(block_id)]
            .read()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = shard.map.get(&block_id) {
            if !entry.referenced.load(Ordering::Relaxed) {
                entry.referenced.store(true, Ordering::Relaxed);
            }
            dst.copy_from_slice(&entry.data[in_blk_off..in_blk_off + dst.len()]);
            true
        } else {
            false
        }
    }

    fn insert(&self, block_id: u64, block: Box<[u8; BLOCK_SIZE]>) {
        let limit = self.max_per_shard.load(Ordering::Relaxed);
        if limit == 0 {
            return;
        }
        let mut shard = self.shards[Self::shard_idx(block_id)]
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = shard.map.get_mut(&block_id) {
            existing.data = block;
            existing.referenced.store(true, Ordering::Relaxed);
            return;
        }
        let mut net_delta: isize = 1;
        while shard.map.len() >= limit {
            if Self::evict_one_locked(&mut shard).is_some() {
                net_delta -= 1;
            } else {
                break;
            }
        }
        shard.map.insert(
            block_id,
            DataCacheEntry {
                data: block,
                referenced: AtomicBool::new(true),
            },
        );
        shard.ring.push(block_id);
        if net_delta > 0 {
            self.count.fetch_add(net_delta as usize, Ordering::Relaxed);
        } else if net_delta < 0 {
            self.count
                .fetch_sub((-net_delta) as usize, Ordering::Relaxed);
        }
    }

    #[inline]
    fn update_if_present(&self, block_id: u64, offset_in_block: usize, data: &[u8]) {
        if self.count.load(Ordering::Relaxed) == 0 {
            return;
        }
        let mut shard = self.shards[Self::shard_idx(block_id)]
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = shard.map.get_mut(&block_id) {
            let end = (offset_in_block + data.len()).min(BLOCK_SIZE);
            if offset_in_block < end {
                entry.data[offset_in_block..end].copy_from_slice(&data[..end - offset_in_block]);
            }
        }
    }

    #[inline]
    fn invalidate(&self, block_id: u64) {
        if self.count.load(Ordering::Relaxed) == 0 {
            return;
        }
        let mut shard = self.shards[Self::shard_idx(block_id)]
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if shard.map.remove(&block_id).is_some() {
            self.count.fetch_sub(1, Ordering::Relaxed);
        }
    }

    fn invalidate_block_range(&self, first_blk: u64, last_blk: u64) {
        if self.count.load(Ordering::Relaxed) == 0 {
            return;
        }
        let span = last_blk.saturating_sub(first_blk).saturating_add(1);
        if span <= 256 {
            for blk in first_blk..=last_blk {
                self.invalidate(blk);
            }
            return;
        }
        let mut removed = 0usize;
        for lock in &self.shards {
            let mut shard = lock.write().unwrap_or_else(|e| e.into_inner());
            if shard.map.is_empty() {
                continue;
            }
            let before = shard.map.len();
            shard.map.retain(|&k, _| k < first_blk || k > last_blk);
            removed += before - shard.map.len();
        }
        if removed > 0 {
            self.count.fetch_sub(removed, Ordering::Relaxed);
        }
    }

    fn clear(&self) {
        if self.count.load(Ordering::Relaxed) == 0 {
            return;
        }
        for lock in &self.shards {
            let mut shard = lock.write().unwrap_or_else(|e| e.into_inner());
            shard.map.clear();
            shard.ring.clear();
            shard.hand = 0;
        }
        self.count.store(0, Ordering::Relaxed);
    }
}

/// Positional `pread`/`pwrite` storage engine with write-through bitmap/WAL mirrors,
/// a bounded 4 MiB metadata block cache, and a configurable 64-shard user-space data page cache.
/// Never calls `mmap()`.
pub(crate) struct FileBacking {
    file: File,
    len: usize,
    inode_bitmap_off: u64,
    inode_bitmap: Vec<u8>,
    data_bitmap_off: u64,
    data_bitmap: Vec<u8>,
    journal_off: u64,
    journal_buf: Vec<u8>,
    block_cache: MetadataBlockCache,
    data_cache: DataPageCache,
}

impl FileBacking {
    /// Opens a `FileBacking` over an existing or newly sized `File`.
    /// Call [`Self::configure_regions`] once the `SuperBlock` is known so bitmaps and the
    /// optional journal ring are loaded into their write-through mirrors.
    pub(crate) fn new(file: File, len: usize) -> Self {
        Self {
            file,
            len,
            inode_bitmap_off: 0,
            inode_bitmap: Vec::new(),
            data_bitmap_off: 0,
            data_bitmap: Vec::new(),
            journal_off: 0,
            journal_buf: Vec::new(),
            block_cache: MetadataBlockCache::new(),
            data_cache: DataPageCache::new(DataPageCache::default_capacity_bytes()),
        }
    }

    /// Loads bitmap spans and the optional journal region from disk into write-through mirrors.
    pub(crate) fn configure_regions(&mut self, sb: &SuperBlock) -> io::Result<()> {
        let ib_off = sb
            .inode_bitmap_block
            .checked_mul(BLOCK_SIZE as u64)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "inode bitmap offset overflow")
            })?;
        let ib_len = (sb.inode_bitmap_blocks() as usize)
            .checked_mul(BLOCK_SIZE)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "inode bitmap length overflow")
            })?;
        let ib_end = usize::try_from(ib_off)
            .ok()
            .and_then(|o| o.checked_add(ib_len))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "inode bitmap range overflow")
            })?;
        if ib_end > self.len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "inode bitmap region exceeds image size",
            ));
        }
        let mut ib = vec![0u8; ib_len];
        file_read_exact_at(&self.file, ib_off, &mut ib)?;

        let db_off = sb
            .data_bitmap_block
            .checked_mul(BLOCK_SIZE as u64)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "data bitmap offset overflow")
            })?;
        let db_len = (sb.data_bitmap_blocks() as usize)
            .checked_mul(BLOCK_SIZE)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "data bitmap length overflow")
            })?;
        let db_end = usize::try_from(db_off)
            .ok()
            .and_then(|o| o.checked_add(db_len))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "data bitmap range overflow")
            })?;
        if db_end > self.len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "data bitmap region exceeds image size",
            ));
        }
        let mut db = vec![0u8; db_len];
        file_read_exact_at(&self.file, db_off, &mut db)?;

        let (j_off, jb) = if sb.has_journal_layout() {
            let off = sb
                .journal_header_block()
                .checked_mul(BLOCK_SIZE as u64)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "journal offset overflow")
                })?;
            let len = (crate::journal::JOURNAL_RESERVED_BLOCKS as usize) * BLOCK_SIZE;
            let end = usize::try_from(off)
                .ok()
                .and_then(|o| o.checked_add(len))
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "journal range overflow")
                })?;
            if end > self.len {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "journal region exceeds image size",
                ));
            }
            let mut buf = vec![0u8; len];
            file_read_exact_at(&self.file, off, &mut buf)?;
            (off, buf)
        } else {
            (0, Vec::new())
        };

        self.inode_bitmap_off = ib_off;
        self.inode_bitmap = ib;
        self.data_bitmap_off = db_off;
        self.data_bitmap = db;
        self.journal_off = j_off;
        self.journal_buf = jb;
        self.block_cache.clear();
        self.data_cache.clear();
        Ok(())
    }

    /// Synchronizes an arbitrary byte write with the in-memory mirrors, metadata block cache,
    /// and user-space data page cache.
    fn sync_mirrors_on_write(&mut self, offset: u64, src: &[u8]) {
        let write_end = offset.saturating_add(src.len() as u64);

        // Helper to copy overlapping range into a mirror buffer.
        let copy_overlap = |mirror_off: u64, mirror: &mut [u8]| {
            if mirror.is_empty() {
                return;
            }
            let m_end = mirror_off.saturating_add(mirror.len() as u64);
            let ov_start = offset.max(mirror_off);
            let ov_end = write_end.min(m_end);
            if ov_start < ov_end {
                let s_off = (ov_start - offset) as usize;
                let m_off = (ov_start - mirror_off) as usize;
                let n = (ov_end - ov_start) as usize;
                mirror[m_off..m_off + n].copy_from_slice(&src[s_off..s_off + n]);
            }
        };

        copy_overlap(self.inode_bitmap_off, &mut self.inode_bitmap);
        copy_overlap(self.data_bitmap_off, &mut self.data_bitmap);
        copy_overlap(self.journal_off, &mut self.journal_buf);

        // Keep cached 4 KiB metadata and data blocks coherent with [offset..write_end).
        if !src.is_empty() {
            let first_blk = offset / BLOCK_SIZE as u64;
            let last_blk = (write_end - 1) / BLOCK_SIZE as u64;
            let span = last_blk.saturating_sub(first_blk).saturating_add(1);
            if span <= 64 {
                for blk in first_blk..=last_blk {
                    let blk_start = blk * BLOCK_SIZE as u64;
                    let ov_start = offset.max(blk_start);
                    let ov_end = write_end.min(blk_start + BLOCK_SIZE as u64);
                    let s_off = (ov_start - offset) as usize;
                    let b_off = (ov_start - blk_start) as usize;
                    let n = (ov_end - ov_start) as usize;
                    let slice = &src[s_off..s_off + n];
                    self.block_cache.update_if_present(blk, b_off, slice);
                    self.data_cache.update_if_present(blk, b_off, slice);
                }
            } else {
                // Large streaming write (e.g. multi-MiB contiguous payload): avoid O(blocks)
                // per-block shard lock traffic on caches.
                self.block_cache.invalidate_block_range(first_blk, last_blk);
                self.data_cache.invalidate_block_range(first_blk, last_blk);
            }
        }
    }
}

/// Mutable bitmap slice guard that writes dirty bytes through to disk on drop under [`StorageMode::FileIo`]
/// and is a zero-cost wrapper around `&mut [u8]` under [`StorageMode::Mmap`].
pub(crate) struct BitmapSliceMut<'a> {
    slice: &'a mut [u8],
    file_writeback: Option<(&'a File, u64)>,
    /// Optional narrowed dirty byte range `[start..end)` within `slice`.
    /// `None` flushes the entire bitmap span on drop (safe default).
    dirty_range: Option<(usize, usize)>,
}

impl BitmapSliceMut<'_> {
    /// Narrows the `FileIo` writeback on drop to only the modified byte range `[byte_start..byte_end)`.
    #[inline]
    pub(crate) fn set_dirty_range(&mut self, byte_start: usize, byte_end: usize) {
        let len = self.slice.len();
        self.dirty_range = Some((byte_start.min(len), byte_end.min(len)));
    }

    /// Narrows the `FileIo` writeback on drop to the byte range covering bit indices `[start_bit..start_bit + bit_count)`.
    #[inline]
    pub(crate) fn set_dirty_bits(&mut self, start_bit: usize, bit_count: usize) {
        if bit_count == 0 {
            self.dirty_range = Some((0, 0));
            return;
        }
        let byte_start = start_bit / 8;
        let byte_end = start_bit.saturating_add(bit_count).div_ceil(8);
        self.set_dirty_range(byte_start, byte_end);
    }

    /// Explicitly flushes the dirty bitmap bytes to disk under [`StorageMode::FileIo`] and
    /// marks the guard clean so `Drop` will not write a second time.
    pub(crate) fn flush_writeback(&mut self) -> io::Result<()> {
        if let Some((file, offset)) = self.file_writeback {
            let res = match self.dirty_range {
                Some((s, e)) if s < e && e <= self.slice.len() => {
                    file_write_all_at(file, offset + s as u64, &self.slice[s..e])
                }
                Some(_) => Ok(()),
                None => file_write_all_at(file, offset, self.slice),
            };
            self.dirty_range = Some((0, 0));
            res
        } else {
            Ok(())
        }
    }
}

impl Deref for BitmapSliceMut<'_> {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        self.slice
    }
}

impl DerefMut for BitmapSliceMut<'_> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [u8] {
        self.slice
    }
}

impl Drop for BitmapSliceMut<'_> {
    fn drop(&mut self) {
        let _ = self.flush_writeback();
    }
}

#[derive(Clone, Copy)]
enum JournalWriteback {
    /// Flush the full 132 KiB journal region (safe default, e.g. `format_journal`).
    Full,
    /// Read-only access; do not write anything on drop.
    None,
    /// Flush only the 64-byte journal header (`[0..JOURNAL_HEADER_LEN]`).
    HeaderOnly,
    /// Flush the appended WAL frame (plus any wrap-around zeroed tail) first,
    /// followed by the 64-byte journal header.
    Append {
        block_size: usize,
        frame_start: usize,
        frame_end: usize,
        wrap_pad_start: Option<usize>,
    },
}

/// Mutable WAL region guard that writes the modified journal bytes back to disk on drop under
/// [`StorageMode::FileIo`] and is a zero-cost slice wrapper under [`StorageMode::Mmap`].
pub(crate) struct JournalRegionMut<'a> {
    slice: &'a mut [u8],
    file_writeback: Option<(&'a File, u64)>,
    writeback: JournalWriteback,
}

impl JournalRegionMut<'_> {
    /// Marks this borrow as read-only so dropping it performs zero `pwrite` syscalls under `FileIo`.
    #[inline]
    pub(crate) fn mark_clean(&mut self) {
        self.writeback = JournalWriteback::None;
    }

    /// Narrows the `FileIo` writeback on drop to only the 64-byte journal header.
    #[inline]
    pub(crate) fn mark_header_only(&mut self) {
        self.writeback = JournalWriteback::HeaderOnly;
    }

    /// Narrows the `FileIo` writeback on drop to the newly appended WAL frame (and optional
    /// wrap-around zeroed tail) followed by the 64-byte journal header.
    #[inline]
    pub(crate) fn mark_append_written(
        &mut self,
        block_size: usize,
        written: Option<(u64, u64)>,
        wrap_pad_start: Option<u64>,
    ) {
        let (frame_start, frame_end) = match written {
            Some((s, e)) if e > s => (s as usize, e as usize),
            _ => (0, 0),
        };
        self.writeback = JournalWriteback::Append {
            block_size,
            frame_start,
            frame_end,
            wrap_pad_start: wrap_pad_start.map(|p| p as usize),
        };
    }

    /// Explicitly writes any pending WAL bytes through to disk under [`StorageMode::FileIo`]
    /// and marks the guard clean so `Drop` is a no-op.
    pub(crate) fn flush_writeback(&mut self) -> io::Result<()> {
        let wb = std::mem::replace(&mut self.writeback, JournalWriteback::None);
        let Some((file, offset)) = self.file_writeback else {
            return Ok(());
        };
        let total_len = self.slice.len();
        let hdr_len = crate::journal::JOURNAL_HEADER_LEN.min(total_len);
        match wb {
            JournalWriteback::None => Ok(()),
            JournalWriteback::Full => file_write_all_at(file, offset, self.slice),
            JournalWriteback::HeaderOnly => {
                if hdr_len > 0 {
                    file_write_all_at(file, offset, &self.slice[..hdr_len])
                } else {
                    Ok(())
                }
            }
            JournalWriteback::Append {
                block_size,
                frame_start,
                frame_end,
                wrap_pad_start,
            } => {
                // 1. Write zeroed wrap tail (if wrap occurred) before advancing head.
                if let Some(pad) = wrap_pad_start {
                    let abs_s = block_size.saturating_add(pad);
                    if abs_s < total_len {
                        file_write_all_at(file, offset + abs_s as u64, &self.slice[abs_s..])?;
                    }
                }
                // 2. Write the appended frame payload before advancing head.
                if frame_end > frame_start {
                    let abs_s = block_size.saturating_add(frame_start);
                    let abs_e = block_size.saturating_add(frame_end).min(total_len);
                    if abs_s < abs_e {
                        file_write_all_at(file, offset + abs_s as u64, &self.slice[abs_s..abs_e])?;
                    }
                }
                // 3. Write the 64-byte journal header committing the new `head` cursor.
                if hdr_len > 0 {
                    file_write_all_at(file, offset, &self.slice[..hdr_len])?;
                }
                Ok(())
            }
        }
    }
}

impl Deref for JournalRegionMut<'_> {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        self.slice
    }
}

impl DerefMut for JournalRegionMut<'_> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [u8] {
        self.slice
    }
}

impl Drop for JournalRegionMut<'_> {
    fn drop(&mut self) {
        let _ = self.flush_writeback();
    }
}

/// Unified container backing supporting either zero-copy `Mmap` or pure `pread`/`pwrite` `FileIo`.
pub(crate) enum ImageBacking {
    Mmap(MmapMut),
    File(Box<FileBacking>),
}

impl ImageBacking {
    /// Creates an [`ImageBacking`] for the requested [`StorageMode`].
    pub(crate) fn open(file: &File, mode: StorageMode) -> io::Result<Self> {
        match mode {
            StorageMode::Mmap => {
                // SAFETY: caller holds exclusive advisory lock on `file` and validates geometry.
                let mmap = unsafe { MmapOptions::new().map_mut(file)? };
                Ok(Self::Mmap(mmap))
            }
            StorageMode::FileIo => {
                let len = usize::try_from(file.metadata()?.len()).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "image size overflows usize")
                })?;
                let cloned = file.try_clone()?;
                Ok(Self::File(Box::new(FileBacking::new(cloned, len))))
            }
        }
    }

    #[inline]
    pub(crate) fn mode(&self) -> StorageMode {
        match self {
            Self::Mmap(_) => StorageMode::Mmap,
            Self::File(_) => StorageMode::FileIo,
        }
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Mmap(m) => m.len(),
            Self::File(f) => f.len,
        }
    }

    #[inline]
    pub(crate) fn as_mmap_slice(&self) -> Option<&[u8]> {
        match self {
            Self::Mmap(m) => Some(&m[..]),
            Self::File(_) => None,
        }
    }

    /// Configures write-through metadata mirrors (bitmaps + optional WAL) for [`StorageMode::FileIo`].
    /// No-op for [`StorageMode::Mmap`].
    pub(crate) fn configure_regions(&mut self, sb: &SuperBlock) -> io::Result<()> {
        match self {
            Self::Mmap(_) => Ok(()),
            Self::File(f) => f.configure_regions(sb),
        }
    }

    /// Returns `true` when this backing has an active user-space data page cache (`FileIo`
    /// with non-zero cache capacity).
    #[inline]
    pub(crate) fn has_data_page_cache(&self) -> bool {
        match self {
            Self::Mmap(_) => false,
            Self::File(f) => f.data_cache.is_enabled(),
        }
    }

    /// Resizes or disables (`0`) the user-space data page cache under [`StorageMode::FileIo`].
    /// No-op for [`StorageMode::Mmap`].
    pub(crate) fn set_data_cache_capacity_bytes(&self, bytes: usize) {
        if let Self::File(f) = self {
            f.data_cache.set_capacity_bytes(bytes);
        }
    }

    /// Returns the configured user-space data page cache budget in bytes (`0` for `Mmap` or disabled).
    pub(crate) fn data_cache_capacity_bytes(&self) -> usize {
        match self {
            Self::Mmap(_) => 0,
            Self::File(f) => f.data_cache.capacity_bytes(),
        }
    }

    /// Returns the approximate number of bytes currently resident in the user-space data page cache.
    pub(crate) fn data_cache_used_bytes(&self) -> usize {
        match self {
            Self::Mmap(_) => 0,
            Self::File(f) => f.data_cache.used_bytes(),
        }
    }

    /// Non-blocking cache-only read of `[offset..offset + dst.len()]`.
    ///
    /// - Under `Mmap`: copies directly from the memory mapping and returns `Some(Ok(()))`.
    /// - Under `FileIo`: copies from `DataPageCache` if every overlapping 4 KiB block is already
    ///   resident in RAM, returning `Some(Ok(()))`; returns `None` if any block would require a
    ///   blocking `pread` syscall.
    #[inline]
    pub(crate) fn try_read_data_cached_only(
        &self,
        offset: u64,
        dst: &mut [u8],
    ) -> Option<io::Result<()>> {
        if dst.is_empty() {
            return Some(Ok(()));
        }
        let Ok(start) = usize::try_from(offset) else {
            return Some(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "read offset overflow",
            )));
        };
        let Some(end) = start.checked_add(dst.len()) else {
            return Some(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "read offset overflow",
            )));
        };
        if end > self.len() {
            return Some(Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "read past end of image",
            )));
        }
        match self {
            Self::Mmap(m) => {
                dst.copy_from_slice(&m[start..end]);
                Some(Ok(()))
            }
            Self::File(fb) => {
                if !fb.data_cache.is_enabled() || dst.len() > DataPageCache::MAX_CACHED_EXTENT {
                    return None;
                }
                let mut cur_off = offset;
                let mut written = 0usize;
                while written < dst.len() {
                    let blk_id = cur_off / BLOCK_SIZE as u64;
                    let in_blk = (cur_off % BLOCK_SIZE as u64) as usize;
                    let take = (dst.len() - written).min(BLOCK_SIZE - in_blk);
                    let dst_slice = &mut dst[written..written + take];
                    if !fb.data_cache.try_copy_into(blk_id, in_blk, dst_slice) {
                        return None;
                    }
                    written += take;
                    cur_off += take as u64;
                }
                Some(Ok(()))
            }
        }
    }

    /// Non-blocking cache-only read of an 8-byte little-endian `u64` slot from a pointer block.
    /// Returns `None` under `FileIo` if the pointer block is not yet cached in `MetadataBlockCache`.
    #[inline]
    pub(crate) fn try_read_block_ptr_cached_only(
        &self,
        block_id: u64,
        entry_idx: usize,
    ) -> Option<u64> {
        let off = entry_idx * 8;
        if off + 8 > BLOCK_SIZE {
            return Some(0);
        }
        match self {
            Self::Mmap(m) => {
                let start = usize::try_from(block_id)
                    .ok()
                    .and_then(|b| b.checked_mul(BLOCK_SIZE))
                    .and_then(|b| b.checked_add(off))?;
                if start + 8 > m.len() {
                    return Some(0);
                }
                Some(u64::from_le_bytes(m[start..start + 8].try_into().unwrap()))
            }
            Self::File(fb) => fb.block_cache.read_u64_le(block_id, off),
        }
    }

    /// Reads file payload bytes `[offset..offset + dst.len()]`, using the bounded 64-shard
    /// user-space [`DataPageCache`] under [`StorageMode::FileIo`] for extents up to 64 KiB.
    ///
    /// Larger streaming extents bypass the cache with a single contiguous `pread` to preserve
    /// sequential throughput and prevent large scans from evicting hot random-access pages.
    #[inline]
    pub(crate) fn read_data_cached(&self, offset: u64, dst: &mut [u8]) -> io::Result<()> {
        if dst.is_empty() {
            return Ok(());
        }
        let start = usize::try_from(offset)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "read offset overflow"))?;
        let end = start
            .checked_add(dst.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "read offset overflow"))?;
        if end > self.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "read past end of image",
            ));
        }
        match self {
            Self::Mmap(m) => {
                dst.copy_from_slice(&m[start..end]);
                Ok(())
            }
            Self::File(fb) => {
                if !fb.data_cache.is_enabled() || dst.len() > DataPageCache::MAX_CACHED_EXTENT {
                    return file_read_exact_at(&fb.file, offset, dst);
                }
                let mut cur_off = offset;
                let mut written = 0usize;
                while written < dst.len() {
                    let blk_id = cur_off / BLOCK_SIZE as u64;
                    let in_blk = (cur_off % BLOCK_SIZE as u64) as usize;
                    let take = (dst.len() - written).min(BLOCK_SIZE - in_blk);
                    let dst_slice = &mut dst[written..written + take];
                    if !fb.data_cache.try_copy_into(blk_id, in_blk, dst_slice) {
                        let blk_byte_off = blk_id * BLOCK_SIZE as u64;
                        if (blk_byte_off as usize)
                            .checked_add(BLOCK_SIZE)
                            .is_some_and(|e| e <= fb.len)
                        {
                            let mut page = Box::new([0u8; BLOCK_SIZE]);
                            file_read_exact_at(&fb.file, blk_byte_off, &mut page[..])?;
                            dst_slice.copy_from_slice(&page[in_blk..in_blk + take]);
                            fb.data_cache.insert(blk_id, page);
                        } else {
                            file_read_exact_at(&fb.file, cur_off, dst_slice)?;
                        }
                    }
                    written += take;
                    cur_off += take as u64;
                }
                Ok(())
            }
        }
    }

    /// Reads an exact byte range `[offset..offset + dst.len()]`.
    #[inline]
    pub(crate) fn read_exact_at(&self, offset: u64, dst: &mut [u8]) -> io::Result<()> {
        let start = usize::try_from(offset)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "read offset overflow"))?;
        let end = start
            .checked_add(dst.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "read offset overflow"))?;
        if end > self.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "read past end of image",
            ));
        }
        match self {
            Self::Mmap(m) => {
                dst.copy_from_slice(&m[start..end]);
                Ok(())
            }
            Self::File(f) => file_read_exact_at(&f.file, offset, dst),
        }
    }

    /// Writes `src` at `offset`, keeping mirrors and metadata block cache coherent.
    #[inline]
    pub(crate) fn write_all_at(&mut self, offset: u64, src: &[u8]) -> io::Result<()> {
        let start = usize::try_from(offset)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "write offset overflow"))?;
        let end = start
            .checked_add(src.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "write offset overflow"))?;
        if end > self.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "write past end of image",
            ));
        }
        match self {
            Self::Mmap(m) => {
                m[start..end].copy_from_slice(src);
                Ok(())
            }
            Self::File(f) => {
                file_write_all_at(&f.file, offset, src)?;
                f.sync_mirrors_on_write(offset, src);
                Ok(())
            }
        }
    }

    /// Zeroes a single 4 KiB block in place.
    #[inline]
    pub(crate) fn zero_block(&mut self, block_id: u64) -> io::Result<()> {
        let start = usize::try_from(block_id)
            .ok()
            .and_then(|b| b.checked_mul(BLOCK_SIZE))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "block offset overflow"))?;
        let end = start
            .checked_add(BLOCK_SIZE)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "block offset overflow"))?;
        if end > self.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "zero_block past end of image",
            ));
        }
        match self {
            Self::Mmap(m) => {
                m[start..end].fill(0);
                Ok(())
            }
            Self::File(f) => {
                file_write_all_at(&f.file, start as u64, &ZERO_BLOCK)?;
                f.block_cache.invalidate(block_id);
                f.data_cache.invalidate(block_id);
                Ok(())
            }
        }
    }

    /// Copies one 4 KiB block to another 4 KiB block (used by in-place defragmentation).
    pub(crate) fn copy_block(&mut self, src_blk: u64, dst_blk: u64) -> io::Result<()> {
        if src_blk == dst_blk {
            return Ok(());
        }
        let src_off = (src_blk as usize)
            .checked_mul(BLOCK_SIZE)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "src block overflow"))?;
        let dst_off = (dst_blk as usize)
            .checked_mul(BLOCK_SIZE)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "dst block overflow"))?;
        if src_off + BLOCK_SIZE > self.len() || dst_off + BLOCK_SIZE > self.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "block copy out of bounds",
            ));
        }
        match self {
            Self::Mmap(m) => {
                m.copy_within(src_off..src_off + BLOCK_SIZE, dst_off);
                Ok(())
            }
            Self::File(f) => {
                let mut buf = [0u8; BLOCK_SIZE];
                file_read_exact_at(&f.file, src_off as u64, &mut buf)?;
                file_write_all_at(&f.file, dst_off as u64, &buf)?;
                f.block_cache.invalidate(dst_blk);
                f.data_cache.invalidate(dst_blk);
                Ok(())
            }
        }
    }

    /// Borrows a 4 KiB block for reading.
    ///
    /// - Under `Mmap`: zero-copy slice straight from the mapping.
    /// - Under `FileIo`: served from the 16-shard 4 MiB metadata block cache (populating on miss
    ///   via a single `pread`). Shard locks are always released before invoking `f`, so nested
    ///   `with_block` calls are strictly deadlock-free.
    #[inline]
    pub(crate) fn with_block<R>(&self, block_id: u64, f: impl FnOnce(&[u8]) -> R) -> io::Result<R> {
        let start = usize::try_from(block_id)
            .ok()
            .and_then(|b| b.checked_mul(BLOCK_SIZE))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "block offset overflow"))?;
        let end = start
            .checked_add(BLOCK_SIZE)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "block offset overflow"))?;
        if end > self.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "block read past end of image",
            ));
        }
        match self {
            Self::Mmap(m) => Ok(f(&m[start..end])),
            Self::File(fb) => {
                if let Some(arc_blk) = fb.block_cache.get(block_id) {
                    return Ok(f(&arc_blk[..]));
                }
                let mut buf = [0u8; BLOCK_SIZE];
                file_read_exact_at(&fb.file, start as u64, &mut buf)?;
                let arc_blk = Arc::new(buf);
                fb.block_cache.insert(block_id, Arc::clone(&arc_blk));
                Ok(f(&arc_blk[..]))
            }
        }
    }

    /// Borrows a 4 KiB block for read-modify-write mutation.
    ///
    /// - Under `Mmap`: direct `&mut [u8]` into the mapping.
    /// - Under `FileIo`: loads from cache or `pread`, runs `f`, writes the updated block via
    ///   `pwrite`, and updates caches.
    #[inline]
    pub(crate) fn with_block_mut<R>(
        &mut self,
        block_id: u64,
        f: impl FnOnce(&mut [u8]) -> R,
    ) -> io::Result<R> {
        let start = usize::try_from(block_id)
            .ok()
            .and_then(|b| b.checked_mul(BLOCK_SIZE))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "block offset overflow"))?;
        let end = start
            .checked_add(BLOCK_SIZE)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "block offset overflow"))?;
        if end > self.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "block write past end of image",
            ));
        }
        match self {
            Self::Mmap(m) => Ok(f(&mut m[start..end])),
            Self::File(fb) => {
                let mut buf = [0u8; BLOCK_SIZE];
                if let Some(arc_blk) = fb.block_cache.get(block_id) {
                    buf.copy_from_slice(&arc_blk[..]);
                } else {
                    file_read_exact_at(&fb.file, start as u64, &mut buf)?;
                }
                let res = f(&mut buf[..]);
                file_write_all_at(&fb.file, start as u64, &buf)?;
                fb.block_cache.insert(block_id, Arc::new(buf));
                fb.data_cache.invalidate(block_id);
                Ok(res)
            }
        }
    }

    /// Reads an 8-byte little-endian `u64` slot from a pointer block (cached under `FileIo`).
    #[inline]
    pub(crate) fn read_block_ptr(&self, block_id: u64, entry_idx: usize) -> u64 {
        let off = entry_idx * 8;
        if off + 8 > BLOCK_SIZE {
            return 0;
        }
        match self {
            Self::Mmap(m) => {
                let Some(start) = usize::try_from(block_id)
                    .ok()
                    .and_then(|b| b.checked_mul(BLOCK_SIZE))
                    .and_then(|b| b.checked_add(off))
                else {
                    return 0;
                };
                if start + 8 > m.len() {
                    return 0;
                }
                u64::from_le_bytes(m[start..start + 8].try_into().unwrap())
            }
            Self::File(fb) => {
                if let Some(val) = fb.block_cache.read_u64_le(block_id, off) {
                    return val;
                }
                self.with_block(block_id, |slice| {
                    u64::from_le_bytes(slice[off..off + 8].try_into().unwrap())
                })
                .unwrap_or(0)
            }
        }
    }

    /// Writes an 8-byte little-endian `u64` slot into a pointer block without whole-block RMW.
    #[inline]
    pub(crate) fn write_block_ptr(&mut self, block_id: u64, entry_idx: usize, ptr: u64) {
        let off_in_blk = entry_idx * 8;
        if off_in_blk + 8 > BLOCK_SIZE {
            return;
        }
        let Some(blk_start) = usize::try_from(block_id)
            .ok()
            .and_then(|b| b.checked_mul(BLOCK_SIZE))
        else {
            return;
        };
        let abs_off = blk_start + off_in_blk;
        if abs_off + 8 > self.len() {
            return;
        }
        let bytes = ptr.to_le_bytes();
        match self {
            Self::Mmap(m) => {
                m[abs_off..abs_off + 8].copy_from_slice(&bytes);
            }
            Self::File(fb) => {
                if file_write_all_at(&fb.file, abs_off as u64, &bytes).is_ok() {
                    fb.block_cache
                        .update_if_present(block_id, off_in_blk, &bytes);
                    fb.data_cache
                        .update_if_present(block_id, off_in_blk, &bytes);
                }
            }
        }
    }

    /// Reads the inode bitmap span as a contiguous slice.
    #[inline]
    pub(crate) fn inode_bitmap_slice(&self, sb: &SuperBlock) -> Option<&[u8]> {
        match self {
            Self::Mmap(m) => {
                let start = usize::try_from(sb.inode_bitmap_block)
                    .ok()?
                    .checked_mul(BLOCK_SIZE)?;
                let len = usize::try_from(sb.inode_bitmap_blocks())
                    .ok()?
                    .checked_mul(BLOCK_SIZE)?;
                let end = start.checked_add(len)?;
                if end > m.len() {
                    return None;
                }
                Some(&m[start..end])
            }
            Self::File(fb) => {
                if fb.inode_bitmap.is_empty() {
                    None
                } else {
                    Some(&fb.inode_bitmap)
                }
            }
        }
    }

    /// Reads the data bitmap span as a contiguous slice.
    #[inline]
    pub(crate) fn data_bitmap_slice(&self, sb: &SuperBlock) -> Option<&[u8]> {
        match self {
            Self::Mmap(m) => {
                let start = usize::try_from(sb.data_bitmap_block)
                    .ok()?
                    .checked_mul(BLOCK_SIZE)?;
                let len = usize::try_from(sb.data_bitmap_blocks())
                    .ok()?
                    .checked_mul(BLOCK_SIZE)?;
                let end = start.checked_add(len)?;
                if end > m.len() {
                    return None;
                }
                Some(&m[start..end])
            }
            Self::File(fb) => {
                if fb.data_bitmap.is_empty() {
                    None
                } else {
                    Some(&fb.data_bitmap)
                }
            }
        }
    }

    /// Borrows the mutable inode bitmap span (auto-flushed via `pwrite` on drop under `FileIo`).
    #[inline]
    pub(crate) fn inode_bitmap_slice_mut(&mut self, sb: &SuperBlock) -> Option<BitmapSliceMut<'_>> {
        match self {
            Self::Mmap(m) => {
                let start = usize::try_from(sb.inode_bitmap_block)
                    .ok()?
                    .checked_mul(BLOCK_SIZE)?;
                let len = usize::try_from(sb.inode_bitmap_blocks())
                    .ok()?
                    .checked_mul(BLOCK_SIZE)?;
                let end = start.checked_add(len)?;
                if end > m.len() {
                    return None;
                }
                Some(BitmapSliceMut {
                    slice: &mut m[start..end],
                    file_writeback: None,
                    dirty_range: None,
                })
            }
            Self::File(fb) => {
                if fb.inode_bitmap.is_empty() {
                    return None;
                }
                Some(BitmapSliceMut {
                    slice: &mut fb.inode_bitmap,
                    file_writeback: Some((&fb.file, fb.inode_bitmap_off)),
                    dirty_range: None,
                })
            }
        }
    }

    /// Borrows the mutable data bitmap span (auto-flushed via `pwrite` on drop under `FileIo`).
    #[inline]
    pub(crate) fn data_bitmap_slice_mut(&mut self, sb: &SuperBlock) -> Option<BitmapSliceMut<'_>> {
        match self {
            Self::Mmap(m) => {
                let start = usize::try_from(sb.data_bitmap_block)
                    .ok()?
                    .checked_mul(BLOCK_SIZE)?;
                let len = usize::try_from(sb.data_bitmap_blocks())
                    .ok()?
                    .checked_mul(BLOCK_SIZE)?;
                let end = start.checked_add(len)?;
                if end > m.len() {
                    return None;
                }
                Some(BitmapSliceMut {
                    slice: &mut m[start..end],
                    file_writeback: None,
                    dirty_range: None,
                })
            }
            Self::File(fb) => {
                if fb.data_bitmap.is_empty() {
                    return None;
                }
                Some(BitmapSliceMut {
                    slice: &mut fb.data_bitmap,
                    file_writeback: Some((&fb.file, fb.data_bitmap_off)),
                    dirty_range: None,
                })
            }
        }
    }

    /// Borrows the mutable WAL journal region (`JOURNAL_RESERVED_BLOCKS` contiguous blocks).
    pub(crate) fn journal_region_mut(
        &mut self,
        sb: &SuperBlock,
    ) -> Result<JournalRegionMut<'_>, crate::journal::JournalError> {
        let bs = sb.block_size as usize;
        let needed_u64 =
            crate::journal::JOURNAL_RESERVED_BLOCKS.saturating_mul(sb.block_size as u64);
        let available_u64 = self.len() as u64;
        let too_small = || crate::journal::JournalError::RegionTooSmall {
            needed: needed_u64,
            available: available_u64,
        };
        let start = (sb.journal_header_block() as usize)
            .checked_mul(bs)
            .ok_or_else(too_small)?;
        let len = (crate::journal::JOURNAL_RESERVED_BLOCKS as usize)
            .checked_mul(bs)
            .ok_or_else(too_small)?;
        let end = start.checked_add(len).ok_or_else(too_small)?;
        if end > self.len() {
            return Err(too_small());
        }
        match self {
            Self::Mmap(m) => Ok(JournalRegionMut {
                slice: &mut m[start..end],
                file_writeback: None,
                writeback: JournalWriteback::Full,
            }),
            Self::File(fb) => {
                if fb.journal_buf.len() != len {
                    return Err(crate::journal::JournalError::RegionTooSmall {
                        needed: needed_u64,
                        available: fb.journal_buf.len() as u64,
                    });
                }
                Ok(JournalRegionMut {
                    slice: &mut fb.journal_buf,
                    file_writeback: Some((&fb.file, fb.journal_off)),
                    writeback: JournalWriteback::Full,
                })
            }
        }
    }

    /// Borrows the read-only WAL journal region.
    pub(crate) fn journal_region_ref(
        &self,
        sb: &SuperBlock,
    ) -> Result<&[u8], crate::journal::JournalError> {
        let bs = sb.block_size as usize;
        let needed_u64 =
            crate::journal::JOURNAL_RESERVED_BLOCKS.saturating_mul(sb.block_size as u64);
        let available_u64 = self.len() as u64;
        let too_small = || crate::journal::JournalError::RegionTooSmall {
            needed: needed_u64,
            available: available_u64,
        };
        let start = (sb.journal_header_block() as usize)
            .checked_mul(bs)
            .ok_or_else(too_small)?;
        let len = (crate::journal::JOURNAL_RESERVED_BLOCKS as usize)
            .checked_mul(bs)
            .ok_or_else(too_small)?;
        let end = start.checked_add(len).ok_or_else(too_small)?;
        if end > self.len() {
            return Err(too_small());
        }
        match self {
            Self::Mmap(m) => Ok(&m[start..end]),
            Self::File(fb) => {
                if fb.journal_buf.len() != len {
                    return Err(crate::journal::JournalError::RegionTooSmall {
                        needed: needed_u64,
                        available: fb.journal_buf.len() as u64,
                    });
                }
                Ok(&fb.journal_buf)
            }
        }
    }

    /// Applies one committed [`crate::journal::MetadataOp`] directly to the backing store.
    ///
    /// - Under `Mmap`: delegates straight to [`crate::journal::apply_op_in_place`].
    /// - Under `FileIo`: updates the write-through bitmap mirrors, inode slot, or data block
    ///   slice via `pwrite` while enforcing the exact same bounds checks.
    pub(crate) fn apply_journal_op(
        &mut self,
        sb: &SuperBlock,
        op: &crate::journal::MetadataOp,
    ) -> Result<(), crate::disk::DiskManagerError> {
        match self {
            Self::Mmap(m) => Ok(crate::journal::apply_op_in_place(m, sb, op)?),
            Self::File(fb) => {
                use crate::journal::{JournalError, MetadataOp};
                let bs = sb.block_size as u64;
                match op {
                    MetadataOp::SetInodeBitmap {
                        inode_id,
                        allocated,
                    } => {
                        if *inode_id >= sb.inode_count {
                            return Err(JournalError::BlockOutOfRange {
                                block_id: *inode_id,
                            }
                            .into());
                        }
                        let bit = usize::try_from(*inode_id).map_err(|_| {
                            JournalError::BlockOutOfRange {
                                block_id: *inode_id,
                            }
                        })?;
                        let byte_idx = bit / 8;
                        if byte_idx >= fb.inode_bitmap.len() {
                            return Err(JournalError::BlockOutOfRange {
                                block_id: *inode_id,
                            }
                            .into());
                        }
                        let mask = 1u8 << (bit % 8);
                        let new_byte = if *allocated {
                            fb.inode_bitmap[byte_idx] | mask
                        } else {
                            fb.inode_bitmap[byte_idx] & !mask
                        };
                        file_write_all_at(
                            &fb.file,
                            fb.inode_bitmap_off + byte_idx as u64,
                            &[new_byte],
                        )?;
                        fb.inode_bitmap[byte_idx] = new_byte;
                        Ok(())
                    }
                    MetadataOp::SetDataBitmap {
                        block_id,
                        allocated,
                    } => {
                        if *block_id < sb.data_block_start || *block_id >= sb.block_count {
                            return Err(JournalError::BlockOutOfRange {
                                block_id: *block_id,
                            }
                            .into());
                        }
                        let bit = block_id.saturating_sub(sb.data_block_start);
                        let byte_idx = (bit / 8) as usize;
                        if byte_idx >= fb.data_bitmap.len() {
                            return Err(JournalError::BlockOutOfRange {
                                block_id: *block_id,
                            }
                            .into());
                        }
                        let mask = 1u8 << (bit % 8);
                        let new_byte = if *allocated {
                            fb.data_bitmap[byte_idx] | mask
                        } else {
                            fb.data_bitmap[byte_idx] & !mask
                        };
                        file_write_all_at(
                            &fb.file,
                            fb.data_bitmap_off + byte_idx as u64,
                            &[new_byte],
                        )?;
                        fb.data_bitmap[byte_idx] = new_byte;
                        Ok(())
                    }
                    MetadataOp::SetDataBitmapRange {
                        start_block,
                        count,
                        allocated,
                    } => {
                        let end_block = start_block.checked_add(*count as u64).ok_or(
                            JournalError::BlockOutOfRange {
                                block_id: *start_block,
                            },
                        )?;
                        if *start_block < sb.data_block_start || end_block > sb.block_count {
                            return Err(JournalError::BlockOutOfRange {
                                block_id: *start_block,
                            }
                            .into());
                        }
                        let start_bit =
                            usize::try_from(start_block.saturating_sub(sb.data_block_start))
                                .map_err(|_| JournalError::BlockOutOfRange {
                                    block_id: *start_block,
                                })?;
                        let count_usize =
                            usize::try_from(*count).map_err(|_| JournalError::BlockOutOfRange {
                                block_id: *start_block,
                            })?;
                        let end_bit = start_bit.checked_add(count_usize).ok_or(
                            JournalError::BlockOutOfRange {
                                block_id: *start_block,
                            },
                        )?;
                        if end_bit > fb.data_bitmap.len().saturating_mul(8) {
                            return Err(JournalError::BlockOutOfRange {
                                block_id: *start_block,
                            }
                            .into());
                        }
                        if count_usize == 0 {
                            return Ok(());
                        }
                        let byte_start = start_bit / 8;
                        let byte_end = end_bit.div_ceil(8);
                        let prev = fb.data_bitmap[byte_start..byte_end].to_vec();
                        {
                            let mut bm = crate::bitmap::Bitmap::new(&mut fb.data_bitmap);
                            if *allocated {
                                bm.set_range(start_bit, count_usize);
                            } else {
                                bm.clear_range(start_bit, count_usize);
                            }
                        }
                        if let Err(e) = file_write_all_at(
                            &fb.file,
                            fb.data_bitmap_off + byte_start as u64,
                            &fb.data_bitmap[byte_start..byte_end],
                        ) {
                            fb.data_bitmap[byte_start..byte_end].copy_from_slice(&prev);
                            return Err(e.into());
                        }
                        Ok(())
                    }
                    MetadataOp::WriteInode {
                        inode_id,
                        inode_bytes,
                    } => {
                        if *inode_id >= sb.usable_inode_count() {
                            return Err(JournalError::BlockOutOfRange {
                                block_id: *inode_id,
                            }
                            .into());
                        }
                        let table_start = sb.inode_table_block.checked_mul(bs).ok_or(
                            JournalError::BlockOutOfRange {
                                block_id: sb.inode_table_block,
                            },
                        )?;
                        let rel = inode_id
                            .checked_mul(crate::inode_format::INODE_SLOT_SIZE as u64)
                            .ok_or(JournalError::BlockOutOfRange {
                                block_id: *inode_id,
                            })?;
                        let off = table_start
                            .checked_add(rel)
                            .and_then(|v| usize::try_from(v).ok())
                            .ok_or(JournalError::BlockOutOfRange {
                                block_id: *inode_id,
                            })?;
                        let end = off
                            .checked_add(crate::inode_format::INODE_SLOT_SIZE)
                            .ok_or(JournalError::BlockOutOfRange {
                                block_id: *inode_id,
                            })?;
                        if end > fb.len {
                            return Err(JournalError::BlockOutOfRange {
                                block_id: *inode_id,
                            }
                            .into());
                        }
                        file_write_all_at(&fb.file, off as u64, &inode_bytes[..])?;
                        fb.sync_mirrors_on_write(off as u64, &inode_bytes[..]);
                        Ok(())
                    }
                    MetadataOp::WriteBlockSlice {
                        block_id,
                        offset,
                        data,
                    } => {
                        if *block_id < sb.data_block_start || *block_id >= sb.block_count {
                            return Err(JournalError::BlockOutOfRange {
                                block_id: *block_id,
                            }
                            .into());
                        }
                        let slice_off = usize::try_from(*offset).map_err(|_| {
                            JournalError::BlockOutOfRange {
                                block_id: *block_id,
                            }
                        })?;
                        let slice_end = slice_off.checked_add(data.len()).ok_or(
                            JournalError::BlockOutOfRange {
                                block_id: *block_id,
                            },
                        )?;
                        if slice_end > bs as usize {
                            return Err(JournalError::BlockOutOfRange {
                                block_id: *block_id,
                            }
                            .into());
                        }
                        let blk_start = block_id
                            .checked_mul(bs)
                            .and_then(|v| usize::try_from(v).ok())
                            .ok_or(JournalError::BlockOutOfRange {
                                block_id: *block_id,
                            })?;
                        let abs_start = blk_start.checked_add(slice_off).ok_or(
                            JournalError::BlockOutOfRange {
                                block_id: *block_id,
                            },
                        )?;
                        let abs_end = abs_start.checked_add(data.len()).ok_or(
                            JournalError::BlockOutOfRange {
                                block_id: *block_id,
                            },
                        )?;
                        if abs_end > fb.len {
                            return Err(JournalError::BlockOutOfRange {
                                block_id: *block_id,
                            }
                            .into());
                        }
                        file_write_all_at(&fb.file, abs_start as u64, data)?;
                        fb.block_cache.update_if_present(*block_id, slice_off, data);
                        fb.data_cache.update_if_present(*block_id, slice_off, data);
                        Ok(())
                    }
                }
            }
        }
    }

    /// Synchronously flushes the entire image to persistent storage.
    #[inline]
    pub(crate) fn flush(&self) -> io::Result<()> {
        match self {
            Self::Mmap(m) => m.flush(),
            Self::File(f) => f.file.sync_data(),
        }
    }

    /// Asynchronously schedules dirty pages for writeback.
    #[inline]
    pub(crate) fn flush_async(&self) -> io::Result<()> {
        match self {
            Self::Mmap(m) => m.flush_async(),
            Self::File(_) => Ok(()),
        }
    }

    /// Synchronously flushes a byte range `[offset..offset + len]`.
    #[inline]
    pub(crate) fn flush_range(&self, offset: usize, len: usize) -> io::Result<()> {
        match self {
            Self::Mmap(m) => m.flush_range(offset, len),
            Self::File(f) => f.file.sync_data(),
        }
    }

    /// Asynchronously schedules a byte range `[offset..offset + len]` for writeback.
    #[inline]
    pub(crate) fn flush_async_range(&self, offset: usize, len: usize) -> io::Result<()> {
        match self {
            Self::Mmap(m) => m.flush_async_range(offset, len),
            Self::File(_) => Ok(()),
        }
    }

    #[cfg(test)]
    pub(crate) fn replace_file_for_test(&mut self, file: File) {
        if let Self::File(fb) = self {
            fb.file = file;
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    /// Formally prove that `BitmapSliceMut::set_dirty_bits` computes a valid byte range
    /// `[byte_start..byte_end)` that never exceeds `slice.len()` and covers every bit in
    /// `[start_bit..start_bit + bit_count)` whenever the bit range lies within the bitmap.
    #[kani::proof]
    fn proof_bitmap_slice_mut_set_dirty_bits_coverage_and_bounds() {
        let mut buf = [0u8; 64];
        let slice_len: usize = kani::any();
        kani::assume(slice_len <= buf.len());
        let start_bit: usize = kani::any();
        let bit_count: usize = kani::any();

        let mut bm = BitmapSliceMut {
            slice: &mut buf[..slice_len],
            file_writeback: None,
            dirty_range: None,
        };
        bm.set_dirty_bits(start_bit, bit_count);
        let (byte_start, byte_end) = bm.dirty_range.expect("dirty_range must be populated");

        assert!(byte_start <= slice_len);
        assert!(byte_end <= slice_len);

        if bit_count == 0 {
            assert_eq!((byte_start, byte_end), (0, 0));
            kani::cover!(true, "Zero bit_count produces empty dirty range");
        } else {
            assert!(byte_start <= byte_end);
            if let Some(end_bit) = start_bit.checked_add(bit_count)
                && end_bit <= slice_len * 8
            {
                let probe_bit: usize = kani::any();
                kani::assume(probe_bit >= start_bit && probe_bit < end_bit);
                let probe_byte = probe_bit / 8;
                assert!(probe_byte >= byte_start && probe_byte < byte_end);
                kani::cover!(byte_end - byte_start == 1, "Single-byte dirty bit range");
                kani::cover!(byte_end - byte_start > 1, "Multi-byte dirty bit range");
            }
        }
    }

    /// Formally prove that `MetadataBlockCache::shard_idx` (`< 16`) and
    /// `DataPageCache::shard_idx` (`< 64`) are strictly in bounds for every `u64` block ID,
    /// and that the Fibonacci step `h.wrapping_add(0x9E37_79B9_7F4A_7C15)` between
    /// consecutive block IDs never collides in the top 4 bits (16 shards) or top 6 bits (64 shards).
    #[kani::proof]
    fn proof_storage_cache_shard_idx_bounds_and_mixing() {
        let block_id: u64 = kani::any();
        let meta_s = MetadataBlockCache::shard_idx(block_id);
        let data_s = DataPageCache::shard_idx(block_id);
        assert!(meta_s < MetadataBlockCache::NUM_SHARDS);
        assert!(data_s < DataPageCache::NUM_SHARDS);

        // Because (block_id + 1) * C == block_id * C + C (mod 2^64) and gcd(C, 2^64) == 1,
        // consecutive block IDs advance the 64-bit product `h` by C = 0x9E37_79B9_7F4A_7C15.
        let h: u64 = kani::any();
        let h_next = h.wrapping_add(0x9E37_79B9_7F4A_7C15);
        assert_ne!((h >> 60) as usize, (h_next >> 60) as usize);
        assert_ne!((h >> 58) as usize, (h_next >> 58) as usize);
    }

    /// Formally prove that `DataPageCache::blocks_per_shard_for_bytes` clamps against
    /// `MAX_FILE_IO_CACHE_BYTES` and cannot overflow when computing total cache capacity.
    #[kani::proof]
    fn proof_data_page_cache_blocks_per_shard_clamping_and_no_overflow() {
        let bytes: usize = kani::any();
        let per_shard = DataPageCache::blocks_per_shard_for_bytes(bytes);
        let max_per_shard = DataPageCache::blocks_per_shard_for_bytes(MAX_FILE_IO_CACHE_BYTES);

        assert!(per_shard <= max_per_shard);
        let total_blocks = per_shard.checked_mul(DataPageCache::NUM_SHARDS).unwrap();
        let total_bytes = total_blocks.checked_mul(BLOCK_SIZE).unwrap();
        assert!(total_bytes <= MAX_FILE_IO_CACHE_BYTES);

        if bytes < BLOCK_SIZE {
            assert_eq!(per_shard, 0);
        } else {
            assert!(per_shard >= 1);
        }
        if bytes >= MAX_FILE_IO_CACHE_BYTES {
            assert_eq!(per_shard, max_per_shard);
        }
    }
}
