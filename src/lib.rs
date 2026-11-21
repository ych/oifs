pub mod allocator;
pub mod auto_defrag;
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
pub mod storage;
pub mod superblock;
pub mod zstd_pool;

pub use auto_defrag::{AutoDefragConfig, AutoDefragStats};
pub use disk::{
    DiskManager, DiskManagerError, DurabilityMode, FileReadHandle, OifsAsyncTask, ReadRequest,
    ReadSession,
};
pub use filters::{FilterConfig, FilterPipeline, FilterType};
pub use inode_format::INODE_FORMAT_V2;
pub use io_engine::IoBackend;
pub use ipc::{MasterInfo, SessionEvent, SessionMode};
pub use journal::{JournalState, MetadataOp, Transaction};
pub use session::{OifsSession, SessionError};
pub use storage::StorageMode;

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

/// Acquire a read lock, recovering from poisoning.
///
/// Same rationale as [`lock_unpoisoned`], for read-only snapshots of state
/// that a panic cannot leave inconsistent.
pub(crate) fn read_unpoisoned<T>(r: &std::sync::RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    r.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Acquire a write lock, recovering from poisoning.
///
/// Only use this where a panic mid-critical-section cannot leave the guarded
/// data inconsistent. The `DiskManager` inner state, for instance, guards
/// on-disk consistency and therefore keeps propagating the poison.
///
/// No call site exists yet (the auto-defrag coordinator only needs a read-only
/// snapshot of the superblock), so the helper is kept for symmetry with
/// [`lock_unpoisoned`] and [`read_unpoisoned`].
#[allow(dead_code)]
pub(crate) fn write_unpoisoned<T>(r: &std::sync::RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    r.write().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Fast non-cryptographic Fibonacci hasher (`0x517cc1b727220a95`) for integer-keyed
/// and internal in-memory hash tables (`FxHashMap` / `FxHashSet`).
pub mod fx_hash {
    use std::collections::{HashMap, HashSet};
    use std::hash::{BuildHasherDefault, Hasher};

    #[derive(Default, Clone)]
    pub struct FxHasher {
        hash: usize,
    }

    const K: usize = 0x517cc1b727220a95usize;

    impl FxHasher {
        #[inline]
        fn add(&mut self, i: usize) {
            self.hash = (self.hash.rotate_left(5) ^ i).wrapping_mul(K);
        }
    }

    impl Hasher for FxHasher {
        #[inline]
        fn write(&mut self, mut bytes: &[u8]) {
            while bytes.len() >= 8 {
                let mut buf = [0u8; 8];
                buf.copy_from_slice(&bytes[..8]);
                self.add(u64::from_ne_bytes(buf) as usize);
                bytes = &bytes[8..];
            }
            if bytes.len() >= 4 {
                let mut buf = [0u8; 4];
                buf.copy_from_slice(&bytes[..4]);
                self.add(u32::from_ne_bytes(buf) as usize);
                bytes = &bytes[4..];
            }
            if bytes.len() >= 2 {
                let mut buf = [0u8; 2];
                buf.copy_from_slice(&bytes[..2]);
                self.add(u16::from_ne_bytes(buf) as usize);
                bytes = &bytes[2..];
            }
            if let Some(&b) = bytes.first() {
                self.add(b as usize);
            }
        }

        #[inline]
        fn write_u8(&mut self, i: u8) {
            self.add(i as usize);
        }

        #[inline]
        fn write_u16(&mut self, i: u16) {
            self.add(i as usize);
        }

        #[inline]
        fn write_u32(&mut self, i: u32) {
            self.add(i as usize);
        }

        #[inline]
        fn write_u64(&mut self, i: u64) {
            self.add(i as usize);
        }

        #[inline]
        fn write_usize(&mut self, i: usize) {
            self.add(i);
        }

        #[inline]
        fn finish(&self) -> u64 {
            self.hash as u64
        }
    }

    pub type FxBuildHasher = BuildHasherDefault<FxHasher>;
    pub type FxHashMap<K, V> = HashMap<K, V, FxBuildHasher>;
    pub type FxHashSet<T> = HashSet<T, FxBuildHasher>;
}

pub use fx_hash::{FxHashMap, FxHashSet};
