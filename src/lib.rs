pub mod allocator;
pub mod bitmap;
pub mod disk;
pub mod encryption;  // NEW: Encryption module
pub mod inode;
pub mod superblock;
pub mod directory;
pub mod ffi;

pub const BLOCK_SIZE: usize = 4096;
