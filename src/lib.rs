pub mod allocator;
pub mod bitmap;
pub mod directory;
pub mod disk;
pub mod encryption;
pub mod ffi;
pub mod filters;
pub mod inode;
pub mod inode_format;
pub mod io_engine;
pub mod ipc;
pub mod journal;
pub mod session;
pub mod superblock;

pub use disk::{DiskManager, DiskManagerError, DurabilityMode, ReadRequest};
pub use filters::{FilterConfig, FilterPipeline, FilterType};
pub use inode_format::INODE_FORMAT_V2;
pub use io_engine::IoBackend;
pub use ipc::{MasterInfo, SessionEvent, SessionMode};
pub use journal::{JournalState, MetadataOp, Transaction};
pub use session::{OifsSession, SessionError};

pub const BLOCK_SIZE: usize = 4096;

/// Lock a mutex, recovering from poisoning.
///
/// Poisoning means some thread panicked while holding the lock. For a lock that
/// only guards reconstructible in-memory state (caches, registries, a mutex used
/// purely to serialize work), the guarded data is still valid — and propagating
/// the poison would leave the subsystem **permanently** unusable, since every
/// later acquisition would fail the same way. One unrelated panic anywhere would
/// brick that subsystem for the life of the process.
///
/// Deliberately **not** a blanket rule: only use this where a panic mid-critical-
/// section cannot leave the data inconsistent. A lock guarding a stream that may
/// be mid-frame, for instance, must propagate the panic instead — recovering there
/// would silently desynchronize the protocol.
pub(crate) fn lock_unpoisoned<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
