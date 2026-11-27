//! Dynamic image capacity management: online O(1) in-place growth, tail truncation shrink,
//! inode-preserving atomic cross-boundary re-layout, Vacuum compaction, and transparent Auto-Grow.

use super::*;
use crate::inode::{ChunkEntry, INODE_FLAG_SEEKABLE_64K};
use crate::inode_format::{INODE_SLOT_SIZE, encode_v2};
use crate::journal::JournalRing;
use crate::storage::{ImageBacking, file_write_all_at};

/// Policy controlling automatic image growth on block or inode exhaustion.
///
/// Disabled by default so fixed-capacity mounts preserve deterministic [`AllocatorError::NoSpace`]
/// behavior unless opted in via [`DiskManager::set_auto_grow`] or `--auto-grow`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AutoGrowConfig {
    /// Whether automatic expansion is enabled on capacity exhaustion.
    pub enabled: bool,
    /// Multiplicative capacity target (`1.1..=4.0`), default `1.5`.
    pub growth_factor: f64,
    /// Minimum byte increment per expansion, default `4 MiB`.
    pub min_increment_bytes: u64,
    /// Optional hard upper cap in bytes (`0` means unbounded).
    pub max_size_bytes: u64,
    /// Allow online atomic re-layout when crossing bitmap or inode-table capacity limits.
    pub allow_relayout: bool,
}

impl Default for AutoGrowConfig {
    fn default() -> Self {
        Self::disabled()
    }
}

impl AutoGrowConfig {
    /// Returns a disabled auto-grow configuration.
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            growth_factor: 1.5,
            min_increment_bytes: 4 * 1024 * 1024,
            max_size_bytes: 0,
            allow_relayout: true,
        }
    }

    /// Returns a default enabled auto-grow configuration (`1.5x`, min `+4 MiB`, unlimited cap).
    pub const fn enabled() -> Self {
        Self {
            enabled: true,
            growth_factor: 1.5,
            min_increment_bytes: 4 * 1024 * 1024,
            max_size_bytes: 0,
            allow_relayout: true,
        }
    }

    /// Sets the multiplicative growth factor (clamped internally to `1.1..=4.0`).
    pub fn with_growth_factor(mut self, factor: f64) -> Self {
        self.growth_factor = factor;
        self
    }

    /// Sets the minimum growth increment in bytes.
    pub fn with_min_increment_bytes(mut self, bytes: u64) -> Self {
        self.min_increment_bytes = bytes;
        self
    }

    /// Sets an explicit upper limit on image size (`0` = no limit).
    pub fn with_max_size_bytes(mut self, max_bytes: u64) -> Self {
        self.max_size_bytes = max_bytes;
        self
    }

    /// Configures whether cross-boundary atomic re-layout (e.g. >128 MiB bitmap expansion
    /// or inode-table growth) is permitted.
    pub fn with_allow_relayout(mut self, allow: bool) -> Self {
        self.allow_relayout = allow;
        self
    }
}

/// Strategy used by a resize or vacuum operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResizeMethod {
    /// Image already matched or exceeded target bounds.
    NoOp,
    /// Zero-relocation in-place metadata + backing extension or tail truncation.
    InPlace,
    /// Inode-preserving streaming copy + atomic rename to rebalance bitmap/table spans.
    AtomicRelayout,
}

/// Summary returned by [`DiskManager::resize`] and [`DiskManager::grow`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizeStats {
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub blocks_before: u64,
    pub blocks_after: u64,
    pub inodes_before: u64,
    pub inodes_after: u64,
    pub method: ResizeMethod,
}

/// Configuration for [`DiskManager::vacuum`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VacuumOptions {
    /// Minimum free data headroom preserved after shrinking (bytes).
    pub min_headroom_bytes: u64,
    /// Percentage of currently used data blocks preserved as extra free headroom (`0..=100`).
    pub headroom_percent: u8,
    /// Minimum target image size floor in bytes (`0` = shrink as small as live data permits).
    pub min_image_bytes: u64,
    /// Whether cross-boundary compaction/metadata-reclaim via atomic re-layout is permitted.
    pub allow_relayout: bool,
}

impl Default for VacuumOptions {
    fn default() -> Self {
        Self {
            min_headroom_bytes: 1024 * 1024,
            headroom_percent: 15,
            min_image_bytes: 0,
            allow_relayout: true,
        }
    }
}

impl VacuumOptions {
    /// Sets the minimum preserved headroom in bytes.
    pub fn with_min_headroom_bytes(mut self, bytes: u64) -> Self {
        self.min_headroom_bytes = bytes;
        self
    }

    /// Sets the headroom percentage relative to currently used blocks.
    pub fn with_headroom_percent(mut self, pct: u8) -> Self {
        self.headroom_percent = pct.min(100);
        self
    }

    /// Sets a lower bound floor on the post-vacuum image byte size.
    pub fn with_min_image_bytes(mut self, bytes: u64) -> Self {
        self.min_image_bytes = bytes;
        self
    }

    /// Enables or disables atomic cross-boundary re-layout during vacuum.
    pub fn with_allow_relayout(mut self, allow: bool) -> Self {
        self.allow_relayout = allow;
        self
    }
}

/// Summary returned by [`DiskManager::vacuum`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VacuumStats {
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub bytes_reclaimed: u64,
    pub blocks_before: u64,
    pub blocks_after: u64,
    pub blocks_relocated: u64,
    pub method: ResizeMethod,
}

/// Computes the target block count for an automatic expansion using `config`, enforcing
/// monotonicity, `min_increment_bytes`, and `max_size_bytes`.
///
/// Returns `None` if `config.enabled` is false or `required_blocks` exceeds `max_size_bytes`.
pub fn compute_auto_grow_target_blocks(
    current_blocks: u64,
    required_blocks: u64,
    config: &AutoGrowConfig,
) -> Option<u64> {
    if !config.enabled {
        return None;
    }
    let factor = if config.growth_factor.is_finite() {
        config.growth_factor.clamp(1.1, 4.0)
    } else {
        1.5
    };
    let min_inc_blocks = config
        .min_increment_bytes
        .div_ceil(BLOCK_SIZE as u64)
        .max(1);
    let scaled_f = (current_blocks as f64) * factor;
    let scaled = if scaled_f.is_finite() && scaled_f > 0.0 && scaled_f < (u64::MAX as f64) {
        scaled_f.ceil() as u64
    } else {
        u64::MAX
    };
    let floor_grown = current_blocks.saturating_add(min_inc_blocks);
    let mut target = scaled.max(floor_grown).max(required_blocks);

    if config.max_size_bytes > 0 {
        let max_blocks = config.max_size_bytes / BLOCK_SIZE as u64;
        if required_blocks > max_blocks || current_blocks >= max_blocks {
            return None;
        }
        target = target.min(max_blocks);
    }
    Some(target.max(required_blocks))
}

struct TempPathGuard(Option<PathBuf>);

impl Drop for TempPathGuard {
    fn drop(&mut self) {
        if let Some(ref p) = self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn next_resize_tmp_path(backing_path: &Path) -> PathBuf {
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    let name = backing_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("oifs.img");
    let tmp_name = format!(".{name}.resize.{pid}.{seq}.tmp");
    match backing_path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(tmp_name),
        _ => PathBuf::from(tmp_name),
    }
}

#[inline]
fn count_used_data_blocks(guard: &DiskManagerInner) -> (u64, Option<u64>) {
    let sb = &guard.superblock;
    let total_data = sb.block_count.saturating_sub(sb.data_block_start) as usize;
    let Some(bm_slice) = guard.data_bitmap_slice() else {
        return (0, None);
    };
    let mut used = 0u64;
    let mut highest_rel: Option<u64> = None;

    let num_words = total_data / 64;
    let rem_bits = total_data % 64;
    let (chunks, _) = bm_slice.as_chunks::<8>();
    for (w_idx, chunk) in chunks.iter().take(num_words).enumerate() {
        let w = u64::from_le_bytes(*chunk);
        if w != 0 {
            used += w.count_ones() as u64;
            let top_bit = 63 - w.leading_zeros() as u64;
            highest_rel = Some((w_idx as u64) * 64 + top_bit);
        }
    }
    if rem_bits > 0 {
        let base = num_words * 64;
        let bm = crate::bitmap::BitmapRef::new(bm_slice);
        for i in base..total_data {
            if bm.get(i) {
                used += 1;
                highest_rel = Some(i as u64);
            }
        }
    }
    (used, highest_rel)
}

#[inline]
fn count_inode_usage(guard: &DiskManagerInner) -> (u64, u64) {
    let usable = guard.superblock.usable_inode_count() as usize;
    let Some(ib_slice) = guard.inode_bitmap_slice() else {
        return (0, 0);
    };
    let mut used = 0u64;
    let mut max_id = 0u64;
    let num_words = usable / 64;
    let rem_bits = usable % 64;
    let (chunks, _) = ib_slice.as_chunks::<8>();
    for (w_idx, chunk) in chunks.iter().take(num_words).enumerate() {
        let w = u64::from_le_bytes(*chunk);
        if w != 0 {
            used += w.count_ones() as u64;
            let top_bit = 63 - w.leading_zeros() as u64;
            max_id = (w_idx as u64) * 64 + top_bit;
        }
    }
    if rem_bits > 0 {
        let base = num_words * 64;
        let bm = crate::bitmap::BitmapRef::new(ib_slice);
        for i in base..usable {
            if bm.get(i) {
                used += 1;
                max_id = i as u64;
            }
        }
    }
    (used, max_id)
}

fn quiesce_and_checkpoint_wal(guard: &mut DiskManagerInner) -> Result<(), DiskManagerError> {
    guard.drain_pending_async_ranges_internal();
    guard.backing.flush()?;
    let sb = guard.superblock;
    if sb.has_journal_layout() {
        let bs = sb.block_size as u64;
        {
            let mut region = guard.backing.journal_region_mut(&sb)?;
            region.mark_header_only();
            let mut ring = JournalRing::open(&mut region, bs)?;
            ring.checkpoint();
            ring.persist();
        }
        DiskManager::sync_journal_region(guard)?;
    }
    Ok(())
}

/// Computes `(desired_data_blocks, target_blocks)` for [`DiskManager::vacuum`] given
/// live `used_blocks` and [`VacuumOptions`].
#[inline]
pub(crate) fn compute_vacuum_target_blocks(
    sb: &SuperBlock,
    used_blocks: u64,
    options: &VacuumOptions,
) -> (u64, u64) {
    let headroom_bytes_blks = options.min_headroom_bytes.div_ceil(BLOCK_SIZE as u64);
    let headroom_pct_blks =
        (used_blocks as u128 * options.headroom_percent.min(100) as u128).div_ceil(100) as u64;
    let desired_data_blocks = used_blocks
        .saturating_add(headroom_bytes_blks.max(headroom_pct_blks))
        .max(1);
    let floor_blocks = options.min_image_bytes.div_ceil(BLOCK_SIZE as u64);
    let target_blocks = sb
        .data_block_start
        .saturating_add(desired_data_blocks)
        .max(floor_blocks);
    (desired_data_blocks, target_blocks)
}

/// Computes `(new_min_tbl_blocks, required_blocks)` when `auto_grow` must cover
/// `(lack_data, lack_inodes)` beyond current free capacity.
/// Returns `None` if `lack_inodes` exceeds `SuperBlock::MAX_TRACKED_INODES`.
#[inline]
pub(crate) fn compute_auto_grow_expansion_targets(
    sb: &SuperBlock,
    lack_data: u64,
    lack_inodes: u64,
) -> Option<(u64, u64)> {
    let cur_tbl_blocks = sb.data_block_start.saturating_sub(sb.inode_table_block);
    let max_tbl_blocks = SuperBlock::MAX_TRACKED_INODES / SuperBlock::INODE_SLOTS_PER_BLOCK;

    let new_min_tbl = if lack_inodes > 0 {
        let need_total_inodes = sb.usable_inode_count().saturating_add(lack_inodes);
        let min_needed_tbl = need_total_inodes.div_ceil(SuperBlock::INODE_SLOTS_PER_BLOCK);
        if min_needed_tbl > max_tbl_blocks {
            return None;
        }
        (cur_tbl_blocks.saturating_mul(2))
            .max(min_needed_tbl)
            .min(max_tbl_blocks)
    } else {
        cur_tbl_blocks
    };

    let extra_tbl_blocks = new_min_tbl.saturating_sub(cur_tbl_blocks);
    let required_blocks = sb
        .block_count
        .saturating_add(lack_data.max(if lack_inodes > 0 { 16 } else { 1 }))
        .saturating_add(extra_tbl_blocks)
        .saturating_add(4);
    Some((new_min_tbl, required_blocks))
}

#[inline]
pub(crate) fn can_merge_relayout_run(
    last: &(u64, u64, u64),
    old_start: u64,
    new_start: u64,
) -> bool {
    last.0.saturating_add(last.2) == old_start && last.1.saturating_add(last.2) == new_start
}

/// Pushes `(old_start, new_start, cnt)` into a fixed-capacity slice buffer + logical length,
/// coalescing adjacent spans where both `old_start` and `new_start` advance monotonically.
#[cfg(kani)]
#[inline]
pub(crate) fn push_relayout_copy_run_into(
    buf: &mut [(u64, u64, u64)],
    len: &mut usize,
    old_start: u64,
    new_start: u64,
    cnt: u64,
) {
    if cnt == 0 {
        return;
    }
    if *len > 0 {
        let last = &mut buf[*len - 1];
        if can_merge_relayout_run(last, old_start, new_start) {
            last.2 = last.2.saturating_add(cnt);
            return;
        }
    }
    if *len < buf.len() {
        buf[*len] = (old_start, new_start, cnt);
        *len += 1;
    }
}

/// Coalesces contiguous `(old_start, new_start, cnt)` copy spans during relayout.
#[inline]
pub(crate) fn push_relayout_copy_run(
    copy_runs: &mut Vec<(u64, u64, u64)>,
    old_start: u64,
    new_start: u64,
    cnt: u64,
) {
    if cnt == 0 {
        return;
    }
    if let Some(last) = copy_runs.last_mut()
        && can_merge_relayout_run(last, old_start, new_start)
    {
        last.2 = last.2.saturating_add(cnt);
    } else {
        copy_runs.push((old_start, new_start, cnt));
    }
}

impl DiskManager {
    /// Returns the total byte size of the OIFS container according to its superblock.
    pub fn image_size_bytes(&self) -> u64 {
        let guard = self.inner.read().unwrap();
        guard.superblock.block_count * guard.superblock.block_size as u64
    }

    /// Configures automatic image expansion on capacity exhaustion.
    pub fn set_auto_grow(&self, config: AutoGrowConfig) {
        let mut guard = self.inner.write().unwrap();
        guard.auto_grow = config;
        self.auto_grow_enabled
            .store(config.enabled, std::sync::atomic::Ordering::Release);
    }

    /// Returns the active [`AutoGrowConfig`] on this mount.
    pub fn auto_grow_config(&self) -> AutoGrowConfig {
        self.inner.read().unwrap().auto_grow
    }

    /// Explicitly grows the container to at least `new_size_bytes` (rounded up to 4 KiB blocks).
    ///
    /// Fails with `InvalidInput` if `new_size_bytes` is smaller than the current image size.
    pub fn grow(&self, new_size_bytes: u64) -> Result<ResizeStats, DiskManagerError> {
        let cur_bytes = self.image_size_bytes();
        let target_blocks = new_size_bytes.div_ceil(BLOCK_SIZE as u64);
        let aligned_bytes = target_blocks
            .checked_mul(BLOCK_SIZE as u64)
            .ok_or_else(|| {
                DiskManagerError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Target size overflows u64",
                ))
            })?;
        if aligned_bytes < cur_bytes {
            return Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "grow() target {} bytes is smaller than current image size {} bytes; use resize() to shrink",
                    aligned_bytes, cur_bytes
                ),
            )));
        }
        self.resize(aligned_bytes)
    }

    /// Resizes the OIFS container to `new_size_bytes` (aligned up to 4 KiB blocks),
    /// supporting both growing and shrinking.
    ///
    /// - When the target block count stays within the current bitmap & inode-table geometry
    ///   ([`SuperBlock::in_place_block_count_range`]), expansion is O(1) zero-copy in-place,
    ///   and shrinking compacts tail blocks only if live data extends into the truncated suffix.
    /// - When crossing a 128 MiB bitmap boundary or rebalancing metadata, performs an online
    ///   inode-preserving atomic re-layout without altering any `inode_id`, encryption AAD,
    ///   or dictionary-compressed bytes.
    pub fn resize(&self, new_size_bytes: u64) -> Result<ResizeStats, DiskManagerError> {
        if self.is_read_only() {
            return Err(DiskManagerError::ReadOnly);
        }
        let target_blocks = new_size_bytes.div_ceil(BLOCK_SIZE as u64);
        let mut guard = self.inner.write().unwrap();
        if guard.read_only {
            return Err(DiskManagerError::ReadOnly);
        }

        let sb = guard.superblock;
        let bytes_before = sb.block_count * sb.block_size as u64;
        let blocks_before = sb.block_count;
        let inodes_before = sb.usable_inode_count();

        if target_blocks == blocks_before {
            return Ok(ResizeStats {
                bytes_before,
                bytes_after: bytes_before,
                blocks_before,
                blocks_after: blocks_before,
                inodes_before,
                inodes_after: inodes_before,
                method: ResizeMethod::NoOp,
            });
        }

        let (min_in_place, max_in_place) = sb.in_place_block_count_range();
        if target_blocks > blocks_before {
            let method = if target_blocks <= max_in_place {
                Self::grow_in_place_locked(&mut guard, target_blocks)?;
                ResizeMethod::InPlace
            } else {
                let min_tbl = sb
                    .data_block_start
                    .saturating_sub(sb.inode_table_block)
                    .max(1);
                Self::relayout_image_locked(&mut guard, target_blocks, min_tbl)?;
                ResizeMethod::AtomicRelayout
            };
            let after_sb = guard.superblock;
            return Ok(ResizeStats {
                bytes_before,
                bytes_after: after_sb.block_count * BLOCK_SIZE as u64,
                blocks_before,
                blocks_after: after_sb.block_count,
                inodes_before,
                inodes_after: after_sb.usable_inode_count(),
                method,
            });
        }

        // Shrinking: determine live footprint and choose between in-place shrink vs relayout.
        let (used_blocks, _) = count_used_data_blocks(&guard);
        let (_, max_alloc_inode) = count_inode_usage(&guard);

        let method = if target_blocks >= min_in_place {
            let new_data_slots = target_blocks.saturating_sub(sb.data_block_start);
            if new_data_slots < used_blocks {
                return Err(DiskManagerError::Allocator(AllocatorError::NoSpace));
            }
            let (_, highest_rel) = count_used_data_blocks(&guard);
            let needs_compaction = highest_rel.is_some_and(|h| h >= new_data_slots);
            if needs_compaction {
                drop(guard);
                self.defragment_inplace_reclaim_or_fallback(target_blocks, true)?;
                guard = self.inner.write().unwrap();
                // If defrag triggered relayout directly to `target_blocks`, we're already done!
                let cur_sb = guard.superblock;
                if cur_sb.block_count == target_blocks {
                    return Ok(ResizeStats {
                        bytes_before,
                        bytes_after: cur_sb.block_count * BLOCK_SIZE as u64,
                        blocks_before,
                        blocks_after: cur_sb.block_count,
                        inodes_before,
                        inodes_after: cur_sb.usable_inode_count(),
                        method: ResizeMethod::AtomicRelayout,
                    });
                }
            }
            Self::shrink_in_place_locked(&mut guard, target_blocks)?;
            ResizeMethod::InPlace
        } else {
            let min_tbl = (max_alloc_inode + 1)
                .div_ceil(SuperBlock::INODE_SLOTS_PER_BLOCK)
                .max(1);
            Self::relayout_image_locked(&mut guard, target_blocks, min_tbl)?;
            ResizeMethod::AtomicRelayout
        };

        let after_sb = guard.superblock;
        Ok(ResizeStats {
            bytes_before,
            bytes_after: after_sb.block_count * BLOCK_SIZE as u64,
            blocks_before,
            blocks_after: after_sb.block_count,
            inodes_before,
            inodes_after: after_sb.usable_inode_count(),
            method,
        })
    }

    fn defragment_inplace_reclaim_or_fallback(
        &self,
        target_blocks: u64,
        allow_relayout: bool,
    ) -> Result<(), DiskManagerError> {
        match Self::defragment_inplace_on(&self.inner) {
            Ok(_) => Ok(()),
            Err(DiskManagerError::Allocator(AllocatorError::NoSpace)) if allow_relayout => {
                let mut guard = self.inner.write().unwrap();
                let (_, max_alloc_inode) = count_inode_usage(&guard);
                let min_tbl = (max_alloc_inode + 1)
                    .div_ceil(SuperBlock::INODE_SLOTS_PER_BLOCK)
                    .max(1);
                Self::relayout_image_locked(&mut guard, target_blocks, min_tbl)?;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Compacts allocated blocks and shrinks the container to `(live_used + headroom)`,
    /// reclaiming unused space back to the host filesystem.
    pub fn vacuum(&self, options: VacuumOptions) -> Result<VacuumStats, DiskManagerError> {
        if self.is_read_only() {
            return Err(DiskManagerError::ReadOnly);
        }
        let mut guard = self.inner.write().unwrap();
        if guard.read_only {
            return Err(DiskManagerError::ReadOnly);
        }

        let sb = guard.superblock;
        let bytes_before = sb.block_count * sb.block_size as u64;
        let blocks_before = sb.block_count;
        let (used_blocks, highest_rel) = count_used_data_blocks(&guard);
        let (_, max_alloc_inode) = count_inode_usage(&guard);

        let headroom_bytes_blks = 0; // computed cleanly via `compute_vacuum_target_blocks`
        let _ = headroom_bytes_blks;
        let (desired_data_blocks, mut target_blocks) =
            compute_vacuum_target_blocks(&sb, used_blocks, &options);
        let floor_blocks = options.min_image_bytes.div_ceil(BLOCK_SIZE as u64);
        let (min_in_place, _max_in_place) = sb.in_place_block_count_range();

        let min_tbl = (max_alloc_inode + 1)
            .div_ceil(SuperBlock::INODE_SLOTS_PER_BLOCK)
            .max(1);

        // Check whether crossing down into fewer data bitmap blocks or slimmer layout is warranted.
        if options.allow_relayout && target_blocks < min_in_place {
            let probe_blocks = target_blocks.max(min_tbl + 40);
            if let Ok(candidate_sb) = SuperBlock::new_v3_with_min_inodes(
                probe_blocks
                    .saturating_add(if sb.has_journal_layout() { 64 } else { 0 })
                    .max(desired_data_blocks + min_tbl + 40),
                sb.has_journal_layout(),
                min_tbl,
            ) {
                let exact_needed = candidate_sb
                    .data_block_start
                    .saturating_add(desired_data_blocks)
                    .max(floor_blocks);
                if exact_needed < blocks_before {
                    let (_, moved) =
                        Self::relayout_image_locked(&mut guard, exact_needed, min_tbl)?;
                    let after_sb = guard.superblock;
                    let bytes_after = after_sb.block_count * BLOCK_SIZE as u64;
                    return Ok(VacuumStats {
                        bytes_before,
                        bytes_after,
                        bytes_reclaimed: bytes_before.saturating_sub(bytes_after),
                        blocks_before,
                        blocks_after: after_sb.block_count,
                        blocks_relocated: moved,
                        method: ResizeMethod::AtomicRelayout,
                    });
                }
            }
        }

        target_blocks = target_blocks.max(min_in_place);
        if target_blocks >= blocks_before {
            return Ok(VacuumStats {
                bytes_before,
                bytes_after: bytes_before,
                bytes_reclaimed: 0,
                blocks_before,
                blocks_after: blocks_before,
                blocks_relocated: 0,
                method: ResizeMethod::NoOp,
            });
        }

        let new_data_slots = target_blocks.saturating_sub(sb.data_block_start);
        let mut blocks_relocated = 0u64;
        if highest_rel.is_some_and(|h| h >= new_data_slots) {
            drop(guard);
            match Self::defragment_inplace_on(&self.inner) {
                Ok(df_stats) => {
                    blocks_relocated = df_stats.bytes_moved / BLOCK_SIZE as u64;
                }
                Err(DiskManagerError::Allocator(AllocatorError::NoSpace))
                    if options.allow_relayout =>
                {
                    let mut g = self.inner.write().unwrap();
                    let (_, moved) = Self::relayout_image_locked(&mut g, target_blocks, min_tbl)?;
                    let after_sb = g.superblock;
                    let bytes_after = after_sb.block_count * BLOCK_SIZE as u64;
                    return Ok(VacuumStats {
                        bytes_before,
                        bytes_after,
                        bytes_reclaimed: bytes_before.saturating_sub(bytes_after),
                        blocks_before,
                        blocks_after: after_sb.block_count,
                        blocks_relocated: moved,
                        method: ResizeMethod::AtomicRelayout,
                    });
                }
                Err(e) => return Err(e),
            }
            guard = self.inner.write().unwrap();
        }

        Self::shrink_in_place_locked(&mut guard, target_blocks)?;
        let after_sb = guard.superblock;
        let bytes_after = after_sb.block_count * BLOCK_SIZE as u64;
        Ok(VacuumStats {
            bytes_before,
            bytes_after,
            bytes_reclaimed: bytes_before.saturating_sub(bytes_after),
            blocks_before,
            blocks_after: after_sb.block_count,
            blocks_relocated,
            method: ResizeMethod::InPlace,
        })
    }

    /// Preemptively grows the image before a write/create/batch if `auto_grow` is enabled
    /// and current free space cannot hold `(needed_data_blocks, needed_inodes)`.
    pub(crate) fn ensure_capacity_for(
        &self,
        needed_data_blocks: u64,
        needed_inodes: u64,
    ) -> Result<(), DiskManagerError> {
        // Lock-free cache-line-isolated fast bypass when auto-grow is disabled (default).
        if !self
            .auto_grow_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Ok(());
        }
        // Fast read-only probe so steady-state writes under enabled auto-grow take zero write locks.
        let grow_req = {
            let guard = self.inner.read().unwrap();
            if guard.read_only || !guard.auto_grow.enabled {
                return Ok(());
            }
            Self::compute_needed_expansion(&guard, needed_data_blocks, needed_inodes)
        };
        let Some(_) = grow_req else {
            return Ok(());
        };

        let mut guard = self.inner.write().unwrap();
        if guard.read_only || !guard.auto_grow.enabled {
            return Ok(());
        }
        match Self::apply_auto_grow_locked(&mut guard, needed_data_blocks, needed_inodes, false) {
            // Preemptive estimates use raw uncompressed sizes; if they hit `max_size_bytes`, let
            // the actual compressed/compact write run and report `NoSpace` only if it still fails.
            Ok(_) | Err(DiskManagerError::Allocator(AllocatorError::NoSpace)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Reactive growth invoked after a write/create fails with `AllocatorError::NoSpace`.
    /// Returns `Ok(true)` if the image grew and the caller should retry.
    pub(crate) fn try_auto_grow_on_nospace(
        &self,
        extra_data_blocks: u64,
        extra_inodes: u64,
    ) -> Result<bool, DiskManagerError> {
        if !self
            .auto_grow_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Ok(false);
        }
        let mut guard = self.inner.write().unwrap();
        if guard.read_only || !guard.auto_grow.enabled {
            return Ok(false);
        }
        Self::apply_auto_grow_locked(&mut guard, extra_data_blocks.max(1), extra_inodes, true)
    }

    fn compute_needed_expansion(
        guard: &DiskManagerInner,
        needed_data_blocks: u64,
        needed_inodes: u64,
    ) -> Option<(u64, u64)> {
        let sb = &guard.superblock;
        let total_data = sb.block_count.saturating_sub(sb.data_block_start);
        let (used_data, _) = count_used_data_blocks(guard);
        let free_data = total_data.saturating_sub(used_data);

        let usable_inodes = sb.usable_inode_count();
        let (used_inodes, _) = count_inode_usage(guard);
        let free_inodes = usable_inodes.saturating_sub(used_inodes);

        let lack_data = needed_data_blocks.saturating_sub(free_data);
        let lack_inodes = needed_inodes.saturating_sub(free_inodes);
        if lack_data == 0 && lack_inodes == 0 {
            return None;
        }
        Some((lack_data, lack_inodes))
    }

    fn apply_auto_grow_locked(
        guard: &mut DiskManagerInner,
        needed_data_blocks: u64,
        needed_inodes: u64,
        force_on_fragmentation: bool,
    ) -> Result<bool, DiskManagerError> {
        let Some((lack_data, lack_inodes)) =
            Self::compute_needed_expansion(guard, needed_data_blocks, needed_inodes)
        else {
            if !force_on_fragmentation {
                return Ok(false);
            }
            // Even when overall free count looks sufficient, contiguous allocation might have
            // hit fragmentation `NoSpace`. Ensure at least `needed_data_blocks` more space.
            let sb = guard.superblock;
            let req = sb.block_count.saturating_add(needed_data_blocks.max(1));
            let Some(target) =
                compute_auto_grow_target_blocks(sb.block_count, req, &guard.auto_grow)
            else {
                return Ok(false);
            };
            let (_, max_in_place) = sb.in_place_block_count_range();
            if target <= max_in_place {
                Self::grow_in_place_locked(guard, target)?;
                return Ok(true);
            } else if guard.auto_grow.allow_relayout {
                let min_tbl = sb
                    .data_block_start
                    .saturating_sub(sb.inode_table_block)
                    .max(1);
                Self::relayout_image_locked(guard, target, min_tbl)?;
                return Ok(true);
            }
            return Ok(false);
        };

        let sb = guard.superblock;
        let cur_tbl_blocks = sb.data_block_start.saturating_sub(sb.inode_table_block);
        let Some((new_min_tbl, required_blocks)) =
            compute_auto_grow_expansion_targets(&sb, lack_data, lack_inodes)
        else {
            return Err(DiskManagerError::Allocator(AllocatorError::NoSpace));
        };
        let _ = cur_tbl_blocks;

        let Some(mut target_blocks) =
            compute_auto_grow_target_blocks(sb.block_count, required_blocks, &guard.auto_grow)
        else {
            return Err(DiskManagerError::Allocator(AllocatorError::NoSpace));
        };

        let (_, max_in_place) = sb.in_place_block_count_range();
        if lack_inodes == 0 && target_blocks <= max_in_place {
            Self::grow_in_place_locked(guard, target_blocks)?;
            return Ok(true);
        }

        if !guard.auto_grow.allow_relayout {
            if lack_inodes == 0 && max_in_place > sb.block_count && max_in_place >= required_blocks
            {
                target_blocks = max_in_place;
                Self::grow_in_place_locked(guard, target_blocks)?;
                return Ok(true);
            }
            return Err(DiskManagerError::Allocator(AllocatorError::NoSpace));
        }

        Self::relayout_image_locked(guard, target_blocks, new_min_tbl)?;
        Ok(true)
    }

    fn grow_in_place_locked(
        guard: &mut DiskManagerInner,
        target_blocks: u64,
    ) -> Result<(), DiskManagerError> {
        let old_blocks = guard.superblock.block_count;
        let db_start = guard.superblock.data_block_start;
        let old_data_blocks = old_blocks.saturating_sub(db_start) as usize;
        let new_data_blocks = target_blocks.saturating_sub(db_start) as usize;
        let new_bytes = target_blocks
            .checked_mul(BLOCK_SIZE as u64)
            .ok_or_else(|| {
                DiskManagerError::Io(std::io::Error::other("Resized byte length overflow"))
            })?;

        // Crash-safety: extend backing file + sync BEFORE persisting larger SuperBlock.
        guard.file.set_len(new_bytes)?;
        crate::storage::crash_sim::record_set_len(&guard.file, new_bytes);
        crate::retry_eintr(|| guard.file.sync_data())?;
        crate::storage::crash_sim::record_sync(&guard.file);
        guard.backing.remap_after_grow(&guard.file, new_bytes)?;

        // Defensively clear bitmap bits for the newly uncovered tail blocks.
        if new_data_blocks > old_data_blocks
            && let Some(mut bm_slice) = guard.data_bitmap_slice_mut()
        {
            let count = new_data_blocks - old_data_blocks;
            {
                let mut bm = crate::bitmap::Bitmap::new(&mut bm_slice);
                bm.clear_range(old_data_blocks, count);
            }
            bm_slice.set_dirty_bits(old_data_blocks, count);
        }

        guard.superblock.block_count = target_blocks;
        Self::persist_superblock(guard)?;
        if guard.durability_mode() != DurabilityMode::Lazy {
            guard.backing.flush_range(0, BLOCK_SIZE)?;
        }
        guard.bump_all_inode_gens();
        Ok(())
    }

    fn shrink_in_place_locked(
        guard: &mut DiskManagerInner,
        target_blocks: u64,
    ) -> Result<(), DiskManagerError> {
        let new_bytes = target_blocks
            .checked_mul(BLOCK_SIZE as u64)
            .ok_or_else(|| {
                DiskManagerError::Io(std::io::Error::other("Resized byte length overflow"))
            })?;

        quiesce_and_checkpoint_wal(guard)?;

        // Crash-safety: persist smaller SuperBlock + flush BEFORE truncating the host file.
        guard.superblock.block_count = target_blocks;
        Self::persist_superblock(guard)?;
        guard.backing.flush()?;
        guard.backing.truncate_and_remap(&guard.file, new_bytes)?;

        let db_start = guard.superblock.data_block_start;
        if guard.free_block_hint >= target_blocks || guard.free_block_hint < db_start {
            guard.free_block_hint = db_start;
        }
        guard.bump_all_inode_gens();
        Ok(())
    }

    /// Online, inode-ID-preserving, streaming atomic re-layout into a sibling temp image file.
    ///
    /// Returns `(new_block_count, blocks_relocated)`.
    fn relayout_image_locked(
        guard: &mut DiskManagerInner,
        target_blocks: u64,
        min_inode_table_blocks: u64,
    ) -> Result<(u64, u64), DiskManagerError> {
        let old_sb = guard.superblock;
        if old_sb.is_legacy_inode_format() {
            return Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Cannot cross-boundary resize a legacy v1 inode image; run `oifs migrate` first",
            )));
        }

        quiesce_and_checkpoint_wal(guard)?;

        let is_journaled = old_sb.has_journal_layout();
        let new_sb = {
            let mut sb = SuperBlock::new_v3_with_min_inodes(
                target_blocks,
                is_journaled,
                min_inode_table_blocks,
            )
            .map_err(|msg| {
                DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, msg))
            })?;
            sb.encrypted = old_sb.encrypted;
            sb.encryption_salt = old_sb.encryption_salt;
            sb.encryption_version = old_sb.encryption_version;
            sb.root_inode = old_sb.root_inode;
            if sb.encrypted
                && let Some(key) = guard.encryption_key.as_ref()
                && (old_sb.has_header_mac()
                    || sb.encryption_version >= crate::encryption::ENCRYPTION_SCHEME_CONTEXT_BOUND)
            {
                sb.header_mac = crate::encryption::compute_superblock_mac(key, &sb);
            }
            sb
        };

        let new_total_bytes = new_sb
            .block_count
            .checked_mul(BLOCK_SIZE as u64)
            .ok_or_else(|| {
                DiskManagerError::Io(std::io::Error::other("Relayout byte length overflow"))
            })?;

        // Collect all currently allocated inode IDs.
        let mut allocated_ids = Vec::new();
        if let Some(ib_slice) = guard.inode_bitmap_slice() {
            let bm = crate::bitmap::BitmapRef::new(ib_slice);
            bm.for_each_set_bit(old_sb.usable_inode_count() as usize, |idx| {
                allocated_ids.push(idx as u64);
            });
        }
        if let Some(&max_id) = allocated_ids.last()
            && max_id >= new_sb.usable_inode_count()
        {
            return Err(DiskManagerError::Allocator(AllocatorError::NoSpace));
        }

        let mut ind_blocks: Vec<(u64, [u8; BLOCK_SIZE])> = Vec::new();
        let mut updated_inodes: Vec<(u64, Inode)> = Vec::with_capacity(allocated_ids.len());

        struct RelayoutState {
            next_target: u64,
            blocks_relocated: u64,
            copy_runs: Vec<(u64, u64, u64)>,
        }

        impl RelayoutState {
            fn alloc_block(&mut self) -> u64 {
                let blk = self.next_target;
                self.next_target += 1;
                self.blocks_relocated += 1;
                blk
            }

            fn remap_leaf(&mut self, guard: &DiskManagerInner, raw: u64, is_seekable: bool) -> u64 {
                if raw == 0 {
                    return 0;
                }
                if is_seekable {
                    let ce = ChunkEntry(raw);
                    if ce.is_empty() {
                        return 0;
                    }
                    let old_start = ce.start_block() as u64;
                    let cnt = ce.block_count() as u64;
                    if cnt == 0
                        || !guard.is_data_block(old_start)
                        || !guard.is_data_block(old_start + cnt - 1)
                    {
                        return 0;
                    }
                    let new_start = self.next_target;
                    self.next_target += cnt;
                    self.blocks_relocated += cnt;
                    push_relayout_copy_run(&mut self.copy_runs, old_start, new_start, cnt);
                    ChunkEntry::new(
                        new_start as u32,
                        ce.block_count(),
                        ce.flags(),
                        ce.compressed_len(),
                    )
                    .0
                } else {
                    if !guard.is_data_block(raw) {
                        return 0;
                    }
                    let new_blk = self.alloc_block();
                    push_relayout_copy_run(&mut self.copy_runs, raw, new_blk, 1);
                    new_blk
                }
            }
        }

        let read_ptrs = |guard: &DiskManagerInner, blk: u64| -> [u64; 512] {
            let mut arr = [0u64; 512];
            if guard.is_data_block(blk) {
                let _ = guard.backing.with_block(blk, |s| {
                    for (dst, chunk) in arr.iter_mut().zip(s.as_chunks::<8>().0.iter()) {
                        *dst = u64::from_le_bytes(*chunk);
                    }
                });
            }
            arr
        };

        let mut rstate = RelayoutState {
            next_target: new_sb.data_block_start,
            blocks_relocated: 0,
            copy_runs: Vec::new(),
        };

        let remap_sib = |rstate: &mut RelayoutState,
                         guard: &DiskManagerInner,
                         old_sib: u64,
                         is_seekable: bool|
         -> Option<(u64, [u8; BLOCK_SIZE])> {
            if !guard.is_data_block(old_sib) {
                return None;
            }
            let ptrs = read_ptrs(guard, old_sib);
            let mut out_bytes = [0u8; BLOCK_SIZE];
            for (i, &raw) in ptrs.iter().enumerate() {
                let mapped = rstate.remap_leaf(guard, raw, is_seekable);
                out_bytes[i * 8..i * 8 + 8].copy_from_slice(&mapped.to_le_bytes());
            }
            let new_sib = rstate.alloc_block();
            Some((new_sib, out_bytes))
        };

        for &inode_id in &allocated_ids {
            let mut inode = Self::read_inode_uncached(guard, inode_id)?;
            let is_seekable = (inode.flags & INODE_FLAG_SEEKABLE_64K) != 0;

            for slot in 0..10 {
                inode.blocks[slot] = rstate.remap_leaf(guard, inode.blocks[slot], is_seekable);
            }

            if inode.blocks[10] != 0 {
                if let Some((new_sib, buf)) =
                    remap_sib(&mut rstate, guard, inode.blocks[10], is_seekable)
                {
                    inode.blocks[10] = new_sib;
                    ind_blocks.push((new_sib, buf));
                } else {
                    inode.blocks[10] = 0;
                }
            }

            if inode.blocks[11] != 0 {
                let old_dib = inode.blocks[11];
                if guard.is_data_block(old_dib) {
                    let dib_ptrs = read_ptrs(guard, old_dib);
                    let mut dib_buf = [0u8; BLOCK_SIZE];
                    for (a, &old_sib) in dib_ptrs.iter().enumerate() {
                        if let Some((new_sib, sib_buf)) =
                            remap_sib(&mut rstate, guard, old_sib, is_seekable)
                        {
                            dib_buf[a * 8..a * 8 + 8].copy_from_slice(&new_sib.to_le_bytes());
                            ind_blocks.push((new_sib, sib_buf));
                        }
                    }
                    let new_dib = rstate.alloc_block();
                    inode.blocks[11] = new_dib;
                    ind_blocks.push((new_dib, dib_buf));
                } else {
                    inode.blocks[11] = 0;
                }
            }

            if inode.triple_indirect != 0 {
                let old_tib = inode.triple_indirect;
                if guard.is_data_block(old_tib) {
                    let tib_ptrs = read_ptrs(guard, old_tib);
                    let mut tib_buf = [0u8; BLOCK_SIZE];
                    for (a, &old_dib) in tib_ptrs.iter().enumerate() {
                        if old_dib != 0 && guard.is_data_block(old_dib) {
                            let dib_ptrs = read_ptrs(guard, old_dib);
                            let mut dib_buf = [0u8; BLOCK_SIZE];
                            for (b, &old_sib) in dib_ptrs.iter().enumerate() {
                                if let Some((new_sib, sib_buf)) =
                                    remap_sib(&mut rstate, guard, old_sib, is_seekable)
                                {
                                    dib_buf[b * 8..b * 8 + 8]
                                        .copy_from_slice(&new_sib.to_le_bytes());
                                    ind_blocks.push((new_sib, sib_buf));
                                }
                            }
                            let new_dib = rstate.alloc_block();
                            tib_buf[a * 8..a * 8 + 8].copy_from_slice(&new_dib.to_le_bytes());
                            ind_blocks.push((new_dib, dib_buf));
                        }
                    }
                    let new_tib = rstate.alloc_block();
                    inode.triple_indirect = new_tib;
                    ind_blocks.push((new_tib, tib_buf));
                } else {
                    inode.triple_indirect = 0;
                }
            }

            updated_inodes.push((inode_id, inode));
        }

        let next_target = rstate.next_target;
        let blocks_relocated = rstate.blocks_relocated;
        let copy_runs = rstate.copy_runs;

        if next_target > new_sb.block_count {
            return Err(DiskManagerError::Allocator(AllocatorError::NoSpace));
        }

        let tmp_path = next_resize_tmp_path(&guard.backing_path);
        let mut tmp_guard = TempPathGuard(Some(tmp_path.clone()));

        let mut open_opts = OpenOptions::new();
        open_opts.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open_opts.mode(0o600);
        }
        let tmp_file = open_opts.open(&tmp_path)?;
        tmp_file.set_len(new_total_bytes)?;
        // Acquire exclusive whole-file lock on `tmp_file` BEFORE atomic rename so no
        // racing external process can ever mount the swapped image unlocked.
        Self::try_lock_whole_file(&tmp_file)?;

        // 1. Copy payload & directory runs from old backing to `tmp_file` (bounded 1 MiB scratch).
        const MAX_COPY_CHUNK_BLOCKS: usize = 256;
        let mut scratch = vec![0u8; MAX_COPY_CHUNK_BLOCKS * BLOCK_SIZE];
        for (old_start, new_start, count) in copy_runs {
            let mut rem = count as usize;
            let mut cur_src = old_start;
            let mut cur_dst = new_start;
            while rem > 0 {
                let step = rem.min(MAX_COPY_CHUNK_BLOCKS);
                let byte_len = step * BLOCK_SIZE;
                guard
                    .backing
                    .read_exact_at(cur_src * BLOCK_SIZE as u64, &mut scratch[..byte_len])?;
                file_write_all_at(&tmp_file, cur_dst * BLOCK_SIZE as u64, &scratch[..byte_len])?;
                cur_src += step as u64;
                cur_dst += step as u64;
                rem -= step;
            }
        }

        // 2. Write rewritten indirect pointer blocks.
        for (new_ind_blk, buf) in ind_blocks {
            file_write_all_at(&tmp_file, new_ind_blk * BLOCK_SIZE as u64, &buf)?;
        }

        // 3. Write all allocated inode slots (preserving every inode_id identically with v3 CRC32C).
        let inode_tbl_off = new_sb.inode_table_block * BLOCK_SIZE as u64;
        let mut new_ib = vec![0u8; new_sb.inode_bitmap_blocks() as usize * BLOCK_SIZE];
        for (inode_id, inode) in &updated_inodes {
            let slot = encode_v2(inode);
            let off = inode_tbl_off + inode_id * INODE_SLOT_SIZE as u64;
            file_write_all_at(&tmp_file, off, &slot)?;
            let mut bm = crate::bitmap::Bitmap::new(&mut new_ib);
            bm.set(*inode_id as usize);
        }

        // 4. Write new inode bitmap and new compacted data bitmap.
        file_write_all_at(
            &tmp_file,
            new_sb.inode_bitmap_block * BLOCK_SIZE as u64,
            &new_ib,
        )?;
        let mut new_db = vec![0u8; new_sb.data_bitmap_blocks() as usize * BLOCK_SIZE];
        let total_used_data = next_target.saturating_sub(new_sb.data_block_start) as usize;
        if total_used_data > 0 {
            let mut bm = crate::bitmap::Bitmap::new(&mut new_db);
            bm.set_range(0, total_used_data);
        }
        file_write_all_at(
            &tmp_file,
            new_sb.data_bitmap_block * BLOCK_SIZE as u64,
            &new_db,
        )?;

        // 5. Write clean WAL region if journaled.
        if is_journaled {
            let mut j_buf =
                vec![0u8; crate::journal::JOURNAL_RESERVED_BLOCKS as usize * BLOCK_SIZE];
            {
                let mut ring = JournalRing::create(&mut j_buf, BLOCK_SIZE as u64)
                    .map_err(DiskManagerError::Journal)?;
                ring.mark_clean_shutdown();
            }
            file_write_all_at(
                &tmp_file,
                new_sb.journal_header_block() * BLOCK_SIZE as u64,
                &j_buf,
            )?;
        }

        // 6. Write SuperBlock at offset 0 and sync all contents before atomic rename.
        let sb_bytes = bincode::serialize(&new_sb)?;
        if sb_bytes.len() > BLOCK_SIZE {
            return Err(DiskManagerError::Serialization(Box::new(
                bincode::ErrorKind::SizeLimit,
            )));
        }
        let mut sb_blk = [0u8; BLOCK_SIZE];
        sb_blk[..sb_bytes.len()].copy_from_slice(&sb_bytes);
        file_write_all_at(&tmp_file, 0, &sb_blk)?;
        crate::retry_eintr(|| tmp_file.sync_all())?;

        // 7. Open new backing and perform atomic filesystem swap.
        let storage_mode = guard.backing.mode();
        let mut new_backing = ImageBacking::open(&tmp_file, storage_mode)?;
        new_backing.configure_regions(&new_sb)?;

        std::fs::rename(&tmp_path, &guard.backing_path)?;
        tmp_guard.0 = None;
        if let Some(parent) = guard.backing_path.parent()
            && !parent.as_os_str().is_empty()
            && let Ok(dir_f) = File::open(parent)
        {
            let _ = dir_f.sync_all();
        }

        // Swap live backing & metadata handles cleanly inside existing exclusive lock.
        guard.file = tmp_file;
        guard.backing = new_backing;
        guard.superblock = new_sb;
        guard.free_block_hint = next_target;
        guard.free_inode_hint = 0;
        guard.inode_cache = BoundedInodeCache::with_capacity(INODE_CACHE_CAPACITY);
        guard.dir_cache.clear();
        guard.clear_payload_caches();
        guard.bump_all_inode_gens();

        Ok((new_sb.block_count, blocks_relocated))
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    #[kani::proof]
    fn proof_auto_grow_target_monotonic_and_capped() {
        let cur: u64 = kani::any();
        let req: u64 = kani::any();
        let max_bytes: u64 = kani::any();
        let min_inc: u64 = kani::any();
        kani::assume((100..=100_000).contains(&cur));
        kani::assume(req >= cur && req <= 200_000);
        kani::assume(min_inc <= 64 * 1024 * 1024);
        kani::assume(max_bytes <= 200_000 * 4096);

        let cfg = AutoGrowConfig::enabled()
            .with_min_increment_bytes(min_inc)
            .with_max_size_bytes(max_bytes);
        if let Some(target) = compute_auto_grow_target_blocks(cur, req, &cfg) {
            assert!(target >= req);
            assert!(target > cur);
            if max_bytes > 0 {
                assert!(target <= max_bytes / 4096);
            }
        } else if max_bytes > 0 {
            let max_blocks = max_bytes / 4096;
            assert!(req > max_blocks || cur >= max_blocks);
        }
    }

    #[kani::proof]
    fn proof_vacuum_target_blocks_sound_and_no_overflow() {
        let total_blocks: u64 = kani::any();
        let journaled: bool = kani::any();
        kani::assume((128..=500_000).contains(&total_blocks));
        let sb = SuperBlock::new_v3(total_blocks, journaled);

        let total_data = sb.block_count.saturating_sub(sb.data_block_start);
        let used_blocks: u64 = kani::any();
        kani::assume(used_blocks <= total_data);

        let min_headroom_bytes: u64 = kani::any();
        let headroom_percent: u8 = kani::any();
        let min_image_bytes: u64 = kani::any();
        let options = VacuumOptions::default()
            .with_min_headroom_bytes(min_headroom_bytes)
            .with_headroom_percent(headroom_percent)
            .with_min_image_bytes(min_image_bytes);

        let (desired_data, target) = compute_vacuum_target_blocks(&sb, used_blocks, &options);
        assert!(desired_data >= used_blocks.max(1));
        assert!(target >= sb.data_block_start.saturating_add(used_blocks.max(1)));
        assert!(target >= options.min_image_bytes.div_ceil(BLOCK_SIZE as u64));
    }

    #[kani::proof]
    fn proof_auto_grow_expansion_targets_sound() {
        let total_blocks: u64 = kani::any();
        let journaled: bool = kani::any();
        kani::assume((128..=500_000).contains(&total_blocks));
        let sb = SuperBlock::new_v3(total_blocks, journaled);

        let lack_data: u64 = kani::any();
        let lack_inodes: u64 = kani::any();
        kani::assume(lack_data <= 100_000);
        kani::assume(lack_inodes <= 40_000);
        kani::assume(lack_data > 0 || lack_inodes > 0);

        let need_inodes = sb.usable_inode_count().saturating_add(lack_inodes);
        let cur_tbl_blocks = sb.data_block_start.saturating_sub(sb.inode_table_block);
        let max_tbl_blocks = SuperBlock::MAX_TRACKED_INODES / SuperBlock::INODE_SLOTS_PER_BLOCK;

        match compute_auto_grow_expansion_targets(&sb, lack_data, lack_inodes) {
            None => {
                assert!(lack_inodes > 0);
                assert!(need_inodes.div_ceil(SuperBlock::INODE_SLOTS_PER_BLOCK) > max_tbl_blocks);
            }
            Some((new_min_tbl, required_blocks)) => {
                assert!(new_min_tbl >= cur_tbl_blocks);
                assert!(new_min_tbl <= max_tbl_blocks);
                if lack_inodes > 0 {
                    assert!(
                        new_min_tbl.saturating_mul(SuperBlock::INODE_SLOTS_PER_BLOCK)
                            >= need_inodes
                    );
                }
                assert!(required_blocks > sb.block_count);
            }
        }
    }

    #[kani::proof]
    #[kani::unwind(5)]
    fn proof_relayout_copy_run_coalescing_conservation() {
        let mut runs_buf = [(0u64, 0u64, 0u64); 3];
        let mut run_len = 0usize;
        let mut next_target = 64u64;
        let mut total_in = 0u64;

        for _ in 0..3 {
            let old_start = (kani::any::<u16>() as u64) & 0xFF;
            let cnt = ((kani::any::<u8>() as u64) % 16) + 1;
            push_relayout_copy_run_into(&mut runs_buf, &mut run_len, old_start, next_target, cnt);
            next_target += cnt;
            total_in += cnt;
        }

        let runs = &runs_buf[..run_len];
        assert!(!runs.is_empty() && runs.len() <= 3);
        let mut total_out = 0u64;
        for (idx, r) in runs.iter().enumerate() {
            assert!(r.2 > 0);
            total_out += r.2;
            if idx > 0 {
                let prev = runs[idx - 1];
                assert_eq!(prev.1 + prev.2, r.1);
                assert_ne!(prev.0 + prev.2, r.0);
            }
        }
        assert_eq!(total_in, total_out);
        assert_eq!(next_target, 64 + total_out);
    }

    #[kani::proof]
    fn proof_auto_grow_arbitrary_float_and_disabled_safe() {
        let cur: u64 = kani::any();
        let req: u64 = kani::any();
        let factor: f64 = kani::any();
        let min_inc: u64 = kani::any();
        let max_bytes: u64 = kani::any();
        let enabled: bool = kani::any();
        kani::assume((100..=100_000).contains(&cur));
        kani::assume(req >= cur && req <= 200_000);
        kani::assume(min_inc <= 64 * 1024 * 1024);
        kani::assume(max_bytes <= 200_000 * 4096);

        let mut cfg = AutoGrowConfig::enabled()
            .with_growth_factor(factor)
            .with_min_increment_bytes(min_inc)
            .with_max_size_bytes(max_bytes);
        cfg.enabled = enabled;

        let res = compute_auto_grow_target_blocks(cur, req, &cfg);
        if !enabled {
            assert_eq!(res, None);
        } else if let Some(target) = res {
            assert!(target >= req);
            assert!(target > cur);
            if max_bytes > 0 {
                assert!(target <= max_bytes / 4096);
            }
        }
    }
}
