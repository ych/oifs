pub mod allocator;
pub mod bitmap;
pub mod disk;
pub mod encryption;
pub mod inode;
pub mod superblock;
pub mod directory;
pub mod ffi;
pub mod ipc;
pub mod session;

pub use session::{OifsSession, SessionError};
pub use ipc::{SessionEvent, SessionMode, MasterInfo};

pub const BLOCK_SIZE: usize = 4096;

