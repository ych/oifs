//! Block allocator module for managing data block allocation
//!
//! This module provides a bitmap-based block allocator that tracks
//! which blocks are free and which are in use.

use thiserror::Error;

/// Errors that can occur during block allocation operations
#[derive(Error, Debug)]
pub enum AllocatorError {
    /// No free blocks available
    #[error("No space left")]
    NoSpace,
    /// I/O error occurred
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Trait for block allocation and deallocation
pub trait BlockAllocator {
    /// Allocates a free block and returns its ID
    ///
    /// # Errors
    /// Returns `AllocatorError::NoSpace` if no free blocks are available
    fn allocate(&mut self) -> Result<u64, AllocatorError>;

    /// Frees a previously allocated block
    ///
    /// # Arguments
    /// * `block_id` - The ID of the block to free
    fn free(&mut self, block_id: u64) -> Result<(), AllocatorError>;
}

use crate::bitmap::Bitmap;

/// Simple bitmap-based block allocator
///
/// Uses a bitmap to track block allocation status. Each bit represents
/// one block: 0 = free, 1 = allocated.
pub struct SimpleBlockAllocator<'a> {
    /// Mutable reference to the bitmap data
    bitmap_data: &'a mut [u8],
    /// Block ID corresponding to bit 0 in the bitmap
    start_block_offset: u64,
}

impl<'a> SimpleBlockAllocator<'a> {
    /// Creates a new block allocator
    ///
    /// # Arguments
    /// * `bitmap_data` - Mutable slice containing the bitmap
    /// * `start_block_offset` - Block ID corresponding to bit 0
    ///
    /// # Example
    /// If `start_block_offset` is 1027, then bit 0 represents block 1027,
    /// bit 1 represents block 1028, etc.
    pub fn new(bitmap_data: &'a mut [u8], start_block_offset: u64) -> Self {
        Self {
            bitmap_data,
            start_block_offset,
        }
    }

    /// Allocates a free block starting from an optional hint block ID.
    ///
    /// Avoids O(N^2) search overhead when sequentially allocating multiple blocks.
    pub fn allocate_with_hint(
        &mut self,
        hint_block_id: Option<u64>,
    ) -> Result<u64, AllocatorError> {
        let hint_bit = hint_block_id
            .and_then(|id| id.checked_sub(self.start_block_offset))
            .map(|bit| bit as usize)
            .unwrap_or(0);

        let mut bitmap = Bitmap::new(self.bitmap_data);
        if let Some(bit_index) = bitmap.find_next_free_wrapped(hint_bit) {
            bitmap.set(bit_index);
            let block_id = self.start_block_offset + bit_index as u64;
            Ok(block_id)
        } else {
            Err(AllocatorError::NoSpace)
        }
    }
}

impl<'a> BlockAllocator for SimpleBlockAllocator<'a> {
    fn allocate(&mut self) -> Result<u64, AllocatorError> {
        self.allocate_with_hint(None)
    }

    fn free(&mut self, block_id: u64) -> Result<(), AllocatorError> {
        // Sanity check: block ID should be >= start offset
        if block_id < self.start_block_offset {
            // Invalid block ID, but don't fail - just ignore
            return Ok(());
        }

        // Convert block ID back to bit index
        let bit_index = (block_id - self.start_block_offset) as usize;

        // Clear the bit to mark block as free
        let mut bitmap = Bitmap::new(self.bitmap_data);
        bitmap.clear(bit_index);

        Ok(())
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    /// Prove that allocate() returns a block ID >= start_block_offset.
    #[kani::proof]
    fn proof_allocate_returns_valid_id() {
        let mut data = [0u8; 2]; // 16 allocatable blocks
        let offset: u64 = kani::any();
        kani::assume(offset < 1024); // reasonable bound

        let mut alloc = SimpleBlockAllocator::new(&mut data, offset);
        if let Ok(block_id) = alloc.allocate() {
            assert!(block_id >= offset, "Block ID must be >= start_block_offset");
            assert!(
                block_id < offset + 16,
                "Block ID must be within bitmap range"
            );
        }
    }

    /// Prove that allocate followed by free makes the block allocatable again.
    #[kani::proof]
    fn proof_allocate_free_roundtrip() {
        let mut data = [0u8; 1]; // 8 blocks
        let mut alloc = SimpleBlockAllocator::new(&mut data, 100);

        // Allocate a block
        let block_id = alloc.allocate().unwrap();
        assert_eq!(block_id, 100, "First allocation should be block 100");

        // Free it
        alloc.free(block_id).unwrap();

        // Should be allocatable again (same block)
        let block_id2 = alloc.allocate().unwrap();
        assert_eq!(block_id2, block_id, "Freed block must be re-allocatable");
    }

    /// Prove that two consecutive allocations never return the same block.
    #[kani::proof]
    fn proof_double_allocate_unique() {
        let mut data = [0u8; 1]; // 8 blocks
        let mut alloc = SimpleBlockAllocator::new(&mut data, 0);

        let a = alloc.allocate().unwrap();
        let b = alloc.allocate().unwrap();
        assert!(a != b, "Two allocations must return different block IDs");
    }

    /// Prove that allocating all blocks exhausts the space.
    #[kani::proof]
    fn proof_exhaustion() {
        let mut data = [0u8; 1]; // 8 blocks
        let mut alloc = SimpleBlockAllocator::new(&mut data, 0);

        // Allocate all 8 blocks
        for _ in 0..8 {
            alloc.allocate().unwrap();
        }

        // Next allocation must fail
        let result = alloc.allocate();
        assert!(result.is_err(), "Must return NoSpace when exhausted");
    }

    /// Prove that free() with an out-of-range block ID (below offset) is safe.
    #[kani::proof]
    fn proof_free_below_offset_safe() {
        let mut data = [0u8; 1];
        let offset: u64 = kani::any();
        kani::assume(offset > 0 && offset < 1024);

        let mut alloc = SimpleBlockAllocator::new(&mut data, offset);
        // Freeing a block below the offset should succeed without side effects
        let result = alloc.free(0);
        assert!(result.is_ok());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_block_allocator_basic() {
        let mut bitmap_buf = vec![0u8; 8]; // 64 bits
        let start_offset = 1000u64;
        let mut allocator = SimpleBlockAllocator::new(&mut bitmap_buf, start_offset);

        // Allocate first 3 blocks
        let b0 = allocator.allocate().expect("alloc 0");
        let b1 = allocator.allocate().expect("alloc 1");
        let b2 = allocator.allocate().expect("alloc 2");

        assert_eq!(b0, 1000);
        assert_eq!(b1, 1001);
        assert_eq!(b2, 1002);

        // Free middle block
        allocator.free(b1).expect("free 1");

        // Next allocation should reuse b1
        let b_reused = allocator.allocate().expect("alloc reused");
        assert_eq!(b_reused, 1001);

        // Next allocation should be b3 (1003)
        let b3 = allocator.allocate().expect("alloc 3");
        assert_eq!(b3, 1003);
    }

    #[test]
    fn test_simple_block_allocator_full() {
        let mut bitmap_buf = vec![0u8; 1]; // 8 bits
        let mut allocator = SimpleBlockAllocator::new(&mut bitmap_buf, 0);

        for i in 0..8 {
            let blk = allocator.allocate().expect("alloc within bounds");
            assert_eq!(blk, i as u64);
        }

        // 9th allocation should fail with NoSpace
        let err = allocator.allocate().unwrap_err();
        assert!(matches!(err, AllocatorError::NoSpace));
    }

    #[test]
    fn test_simple_block_allocator_out_of_bounds_free() {
        let mut bitmap_buf = vec![0u8; 4];
        let mut allocator = SimpleBlockAllocator::new(&mut bitmap_buf, 50);

        // Freeing a block ID smaller than start_block_offset should be ignored safely
        assert!(allocator.free(10).is_ok());
    }
}
