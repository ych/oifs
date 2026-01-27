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
}

impl<'a> BlockAllocator for SimpleBlockAllocator<'a> {
    fn allocate(&mut self) -> Result<u64, AllocatorError> {
        // Create bitmap view over the data
        let mut bitmap = Bitmap::new(self.bitmap_data);
        
        // Find the first free bit (0) in the bitmap
        if let Some(bit_index) = bitmap.find_first_free() {
            // Mark the bit as used (set to 1)
            bitmap.set(bit_index);
            
            // Convert bit index to actual block ID
            let block_id = self.start_block_offset + bit_index as u64;
            Ok(block_id)
        } else {
            Err(AllocatorError::NoSpace)
        }
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
