//! Thread-local pooling of the Zstd contexts (P4.26).
//!
//! Every `zstd::stream::encode_all` / `zstd::stream::Decoder::new` allocates
//! and initialises a fresh `CCtx` / `DCtx`, which costs hundreds of kilobytes
//! of internal tables. On the hot paths (one frame per append, one frame per
//! 64 KiB chunk) that allocation dominates the actual compression work.
//!
//! The pool keeps one context per compression level in thread-local storage, so
//! a steady-state workload allocates nothing at all.

use std::cell::RefCell;
use std::collections::HashMap;

thread_local! {
    /// Pooled single-frame compressors, keyed by compression level.
    static COMPRESSORS: RefCell<HashMap<i32, zstd::bulk::Compressor<'static>>> =
        RefCell::new(HashMap::new());
    /// Pooled single-frame decompressor.
    static DECOMPRESSOR: RefCell<Option<zstd::bulk::Decompressor<'static>>> =
        const { RefCell::new(None) };
}

/// Compresses `data` into a single Zstd frame, reusing a pooled context.
///
/// The output is a valid Zstd frame, byte-compatible with what
/// `zstd::stream::encode_all` produces for the same level.
pub fn compress_frame(data: &[u8], level: i32) -> std::io::Result<Vec<u8>> {
    COMPRESSORS.with(|pool| {
        let mut pool = pool.borrow_mut();
        let compressor = match pool.get_mut(&level) {
            Some(c) => c,
            None => {
                let c = zstd::bulk::Compressor::new(level)?;
                pool.insert(level, c);
                pool.get_mut(&level).expect("just inserted")
            }
        };
        compressor.compress(data)
    })
}

/// Decompresses a single Zstd frame into a buffer of exactly `capacity` bytes.
///
/// Returns the number of bytes written. The pooled decompressor avoids the
/// per-call `DCtx` allocation; the caller provides the destination buffer, so
/// the decompressed data is written in a single pass without reallocations.
pub fn decompress_frame_into(source: &[u8], destination: &mut [u8]) -> std::io::Result<usize> {
    DECOMPRESSOR.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(zstd::bulk::Decompressor::new()?);
        }
        let decompressor = slot.as_mut().expect("initialized above");
        decompressor.decompress_to_buffer(source, destination)
    })
}

/// Returns the decompressed size of a single Zstd frame, if it can be derived
/// from the frame header alone.
pub fn decompressed_size_hint(source: &[u8]) -> Option<usize> {
    zstd::bulk::Decompressor::upper_bound(source)
}

/// Releases the pooled contexts of the current thread.
///
/// The contexts are pooled per thread; a thread that processed a single huge
/// frame can release them explicitly.
pub fn release_pooled_contexts() {
    COMPRESSORS.with(|pool| pool.borrow_mut().clear());
    DECOMPRESSOR.with(|cell| {
        *cell.borrow_mut() = None;
    });
}
