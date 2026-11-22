//! Pure-Rust FUSE Kernel Protocol (ABI v7.28) & VFS engine for OIFS.
//!
//! This module implements a zero-external-C-dependency FUSE ABI v7.28 wire parser,
//! frame dispatcher, stateful open-handle table, cross-platform Linux & macOS kernel
//! mount runners (`/dev/fuse` via `fusermount3`/`fusermount` + `SCM_RIGHTS`, and
//! macOS `FUSE-T` / `macFUSE` / `osxfuse`), UTF-8 NFD -> NFC filename normalization,
//! macOS Finder metadata storm protection, and a high-level VFS facade backed
//! directly by [`DiskManager`].
//!
//! Key properties:
//! - **Endianness portable**: every wire structure uses explicit little-endian
//!   `to_le_bytes` / `from_le_bytes` serialization.
//! - **Dual Linux & macOS ABI support**: supports both standard Linux 88-byte
//!   `FuseAttr` (`FuseEntryOut` 128B, `FuseAttrOut` 104B) and macOS/Darwin 104-byte
//!   `FuseAttr` with `crtime`/`crtimensec`/`flags` (`FuseEntryOut` 144B, `FuseAttrOut` 120B),
//!   plus both Linux (8B) and macOS (16B with `position`) xattr request headers.
//! - **Single-FD container**: any number of open FUSE file handles (`fh`) inside
//!   [`FuseHandleTable`] multiplex over the single underlying OIFS image descriptor.
//! - **Strict read-only enforcement**: when mounted read-only (either via
//!   `DiskManager::is_read_only()` or `FuseMountOptions::read_only`), every mutating
//!   operation returns `-libc::EROFS` with zero disk writes.
//! - **Panic-free frame dispatch**: [`OifsFuseFs::dispatch_frame`] validates every
//!   header and payload boundary and never panics on truncated or malformed input.

use crate::BLOCK_SIZE;
use crate::allocator::AllocatorError;
use crate::directory::DirectoryEntry;
use crate::disk::{CompressionMode, DiskManager, DiskManagerError};
use crate::fx_hash::FxHashMap;
use crate::inode::{FileType, Inode};
use std::borrow::Cow;
use std::io::{Read, Write};
use std::os::unix::io::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

// ---------------------------------------------------------------------------
// FUSE Kernel ABI v7.28 constants
// ---------------------------------------------------------------------------

/// FUSE kernel protocol major version.
pub const FUSE_KERNEL_VERSION: u32 = 7;
/// FUSE kernel protocol minor version supported by this engine.
pub const FUSE_KERNEL_MINOR_VERSION: u32 = 28;
/// Reserved FUSE node ID for the filesystem root directory.
pub const FUSE_ROOT_ID: u64 = 1;

/// Wire size of [`FuseInHeader`] in bytes.
pub const FUSE_IN_HEADER_SIZE: usize = 40;
/// Wire size of [`FuseOutHeader`] in bytes.
pub const FUSE_OUT_HEADER_SIZE: usize = 16;
/// Wire size of standard Linux [`FuseAttr`] in bytes.
pub const FUSE_ATTR_SIZE: usize = 88;
/// Wire size of macOS/Darwin (macFUSE / osxfuse) [`FuseAttr`] in bytes.
pub const FUSE_ATTR_MACOS_SIZE: usize = 104;
/// Wire size of standard Linux [`FuseEntryOut`] in bytes (`40 + 88`).
pub const FUSE_ENTRY_OUT_SIZE: usize = 128;
/// Wire size of macOS/Darwin [`FuseEntryOut`] in bytes (`40 + 104`).
pub const FUSE_ENTRY_OUT_MACOS_SIZE: usize = 144;
/// Wire size of standard Linux [`FuseAttrOut`] in bytes (`16 + 88`).
pub const FUSE_ATTR_OUT_SIZE: usize = 104;
/// Wire size of macOS/Darwin [`FuseAttrOut`] in bytes (`16 + 104`).
pub const FUSE_ATTR_OUT_MACOS_SIZE: usize = 120;
/// Wire size of [`FuseOpenIn`] in bytes.
pub const FUSE_OPEN_IN_SIZE: usize = 8;
/// Wire size of [`FuseOpenOut`] in bytes.
pub const FUSE_OPEN_OUT_SIZE: usize = 16;
/// Wire size of [`FuseReadIn`] in bytes.
pub const FUSE_READ_IN_SIZE: usize = 40;
/// Wire size of [`FuseWriteIn`] in bytes.
pub const FUSE_WRITE_IN_SIZE: usize = 40;
/// Wire size of [`FuseWriteOut`] in bytes.
pub const FUSE_WRITE_OUT_SIZE: usize = 8;
/// Wire size of [`FuseCreateIn`] in bytes.
pub const FUSE_CREATE_IN_SIZE: usize = 16;
/// Wire size of [`FuseMkdirIn`] in bytes.
pub const FUSE_MKDIR_IN_SIZE: usize = 8;
/// Wire size of [`FuseSetAttrIn`] in bytes (standard Linux ABI; macOS extends trailing fields).
pub const FUSE_SETATTR_IN_SIZE: usize = 88;
/// Wire size of [`FuseKStatfs`] in bytes.
pub const FUSE_KSTATFS_SIZE: usize = 80;
/// Wire size of [`FuseInitIn`] in bytes.
pub const FUSE_INIT_IN_SIZE: usize = 16;
/// Wire size of [`FuseInitOut`] in bytes (ABI v7.28).
pub const FUSE_INIT_OUT_SIZE: usize = 64;
/// Fixed header size of an on-wire `fuse_dirent` record before the filename bytes.
pub const FUSE_DIRENT_HEADER_SIZE: usize = 24;
/// Wire size of [`FuseReleaseIn`] in bytes.
pub const FUSE_RELEASE_IN_SIZE: usize = 24;
/// Wire size of [`FuseFlushIn`] in bytes.
pub const FUSE_FLUSH_IN_SIZE: usize = 24;
/// Wire size of [`FuseFsyncIn`] in bytes.
pub const FUSE_FSYNC_IN_SIZE: usize = 16;
/// Wire size of [`FuseAccessIn`] in bytes.
pub const FUSE_ACCESS_IN_SIZE: usize = 8;
/// Wire size of [`FuseForgetIn`] in bytes.
pub const FUSE_FORGET_IN_SIZE: usize = 8;
/// Wire size of `fuse_batch_forget_in` header in bytes.
pub const FUSE_BATCH_FORGET_IN_SIZE: usize = 8;
/// Wire size of Linux `fuse_setxattr_in` / `fuse_getxattr_in` headers in bytes.
pub const FUSE_XATTR_IN_SIZE: usize = 8;
/// Wire size of macOS/Darwin `fuse_setxattr_in` / `fuse_getxattr_in` headers in bytes (adds `position` + padding).
pub const FUSE_XATTR_IN_MACOS_SIZE: usize = 16;
/// Wire size of `fuse_getxattr_out` in bytes.
pub const FUSE_GETXATTR_OUT_SIZE: usize = 8;

/// Maximum FUSE request frame accepted over a stream (16 MiB).
pub const FUSE_MAX_FRAME_LEN: usize = 16 * 1024 * 1024;
/// Minimum valid negotiated `max_write` payload size (4 KiB).
pub const FUSE_MIN_MAX_WRITE: u32 = 4096;
/// Legacy Linux (< 4.20, without `FUSE_MAX_PAGES`) maximum `max_write` payload size (128 KiB).
pub const FUSE_LEGACY_MAX_WRITE: u32 = 128 * 1024;
/// Default negotiated `max_write` payload size (1 MiB).
///
/// Dynamically capped on Linux based on kernel `FUSE_MAX_PAGES` support and
/// `/sys/module/fuse/parameters/max_pages_limit` (128 KiB on Linux < 4.20,
/// 1 MiB on Linux 4.20–6.12, up to 4 MiB on Linux 6.13+ when configured).
pub const FUSE_DEFAULT_MAX_WRITE: u32 = 1024 * 1024;
/// Upper bound for configurable `max_write` (4 MiB, matching Linux 6.13+ `max_pages_limit = 1024`).
pub const FUSE_MAX_SUPPORTED_WRITE: u32 = 4 * 1024 * 1024;
/// Default negotiated `max_readahead` size (1 MiB, clamped by kernel `init_in.max_readahead`).
pub const FUSE_DEFAULT_MAX_READAHEAD: u32 = 1024 * 1024;

// FUSE_SETATTR valid bitmask flags.
pub const FATTR_MODE: u32 = 1 << 0;
pub const FATTR_UID: u32 = 1 << 1;
pub const FATTR_GID: u32 = 1 << 2;
pub const FATTR_SIZE: u32 = 1 << 3;
pub const FATTR_ATIME: u32 = 1 << 4;
pub const FATTR_MTIME: u32 = 1 << 5;
pub const FATTR_FH: u32 = 1 << 6;
pub const FATTR_ATIME_NOW: u32 = 1 << 7;
pub const FATTR_MTIME_NOW: u32 = 1 << 8;
pub const FATTR_LOCKOWNER: u32 = 1 << 9;
pub const FATTR_CTIME: u32 = 1 << 10;

// FUSE_INIT capability flags.
pub const FUSE_ASYNC_READ: u32 = 1 << 0;
pub const FUSE_ATOMIC_O_TRUNC: u32 = 1 << 3;
pub const FUSE_BIG_WRITES: u32 = 1 << 5;
pub const FUSE_MAX_PAGES: u32 = 1 << 22;

// FUSE_OPEN / FUSE_CREATE reply flags.
pub const FOPEN_DIRECT_IO: u32 = 1 << 0;
pub const FOPEN_KEEP_CACHE: u32 = 1 << 1;
pub const FOPEN_NONSEEKABLE: u32 = 1 << 2;
pub const FOPEN_CACHE_DIR: u32 = 1 << 3;

// FUSE opcode numeric constants.
pub const FUSE_LOOKUP: u32 = 1;
pub const FUSE_FORGET: u32 = 2;
pub const FUSE_GETATTR: u32 = 3;
pub const FUSE_SETATTR: u32 = 4;
pub const FUSE_MKDIR: u32 = 9;
pub const FUSE_UNLINK: u32 = 10;
pub const FUSE_RMDIR: u32 = 11;
pub const FUSE_RENAME: u32 = 12;
pub const FUSE_OPEN: u32 = 14;
pub const FUSE_READ: u32 = 15;
pub const FUSE_WRITE: u32 = 16;
pub const FUSE_STATFS: u32 = 17;
pub const FUSE_RELEASE: u32 = 18;
pub const FUSE_FSYNC: u32 = 20;
pub const FUSE_SETXATTR: u32 = 21;
pub const FUSE_GETXATTR: u32 = 22;
pub const FUSE_LISTXATTR: u32 = 23;
pub const FUSE_REMOVEXATTR: u32 = 24;
pub const FUSE_FLUSH: u32 = 25;
pub const FUSE_INIT: u32 = 26;
pub const FUSE_OPENDIR: u32 = 27;
pub const FUSE_READDIR: u32 = 28;
pub const FUSE_RELEASEDIR: u32 = 29;
pub const FUSE_FSYNCDIR: u32 = 30;
pub const FUSE_ACCESS: u32 = 34;
pub const FUSE_CREATE: u32 = 35;
pub const FUSE_DESTROY: u32 = 38;
pub const FUSE_BATCH_FORGET: u32 = 42;
pub const FUSE_RENAME2: u32 = 45;
/// `renameat2(2)` flag: fail with `EEXIST` if destination already exists.
pub const FUSE_RENAME_NOREPLACE: u32 = 1 << 0;

// Standard POSIX mode & dirent file type constants (portable values).
const POSIX_S_IFDIR: u32 = 0o040000;
const POSIX_S_IFREG: u32 = 0o100000;
/// Directory entry type for directories (`DT_DIR`).
pub const FUSE_DT_DIR: u32 = 4;
/// Directory entry type for regular files (`DT_REG`).
pub const FUSE_DT_REG: u32 = 8;

/// Target wire ABI layout for platform-dependent FUSE structs (`fuse_attr`, `fuse_entry_out`,
/// `fuse_attr_out`, `fuse_getxattr_in`, `fuse_setxattr_in`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FuseAbiPlatform {
    /// Standard Linux FUSE ABI v7.28 layout (`FuseAttr` = 88B, `FuseEntryOut` = 128B, `FuseAttrOut` = 104B).
    #[default]
    Linux,
    /// macOS/Darwin macFUSE/osxfuse ABI layout (`FuseAttr` = 104B with `crtime`/`crtimensec`/`flags`,
    /// `FuseEntryOut` = 144B, `FuseAttrOut` = 120B).
    MacOs,
}

impl FuseAbiPlatform {
    /// Returns the native FUSE kernel ABI layout of the host operating system.
    #[inline]
    pub const fn host() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::MacOs
        }
        #[cfg(not(target_os = "macos"))]
        {
            Self::Linux
        }
    }

    /// Encoded byte length of `fuse_attr` for this platform ABI.
    #[inline]
    pub const fn attr_size(self) -> usize {
        match self {
            Self::Linux => FUSE_ATTR_SIZE,
            Self::MacOs => FUSE_ATTR_MACOS_SIZE,
        }
    }

    /// Encoded byte length of `fuse_entry_out` for this platform ABI.
    #[inline]
    pub const fn entry_out_size(self) -> usize {
        match self {
            Self::Linux => FUSE_ENTRY_OUT_SIZE,
            Self::MacOs => FUSE_ENTRY_OUT_MACOS_SIZE,
        }
    }

    /// Encoded byte length of `fuse_attr_out` for this platform ABI.
    #[inline]
    pub const fn attr_out_size(self) -> usize {
        match self {
            Self::Linux => FUSE_ATTR_OUT_SIZE,
            Self::MacOs => FUSE_ATTR_OUT_MACOS_SIZE,
        }
    }

    /// Encoded byte length of `fuse_getxattr_in` / `fuse_setxattr_in` for this platform ABI.
    #[inline]
    pub const fn xattr_in_size(self) -> usize {
        match self {
            Self::Linux => FUSE_XATTR_IN_SIZE,
            Self::MacOs => FUSE_XATTR_IN_MACOS_SIZE,
        }
    }
}

/// Discovered macOS userspace FUSE backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MacOsFuseBackend {
    /// Kext-free Apple Silicon NFSv4 backend (`FUSE-T`).
    FuseT(PathBuf),
    /// `macFUSE` kernel extension (`mount_macfuse`).
    MacFuse(PathBuf),
    /// Legacy `osxfuse` kernel extension (`mount_osxfuse`).
    OsxFuse(PathBuf),
}

/// Probes the host filesystem for an installed macOS FUSE backend (`FUSE-T`, `macFUSE`, or `osxfuse`).
pub fn discover_macos_fuse_backend() -> Option<MacOsFuseBackend> {
    let fuse_t_paths = [
        "/Library/Application Support/fuse-t/bin/mount_fuse-t",
        "/usr/local/bin/go-nfsv4",
        "/opt/homebrew/bin/go-nfsv4",
        "/Library/Application Support/fuse-t",
    ];
    for p in fuse_t_paths {
        let path = Path::new(p);
        if path.exists() {
            return Some(MacOsFuseBackend::FuseT(path.to_path_buf()));
        }
    }
    let macfuse = Path::new("/Library/Filesystems/macfuse.fs/Contents/Resources/mount_macfuse");
    if macfuse.exists() {
        return Some(MacOsFuseBackend::MacFuse(macfuse.to_path_buf()));
    }
    let osxfuse = Path::new("/Library/Filesystems/osxfuse.fs/Contents/Resources/mount_osxfuse");
    if osxfuse.exists() {
        return Some(MacOsFuseBackend::OsxFuse(osxfuse.to_path_buf()));
    }
    None
}

/// FUSE Kernel ABI v7.28 opcodes supported by OIFS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum FuseOpcode {
    Lookup = FUSE_LOOKUP,
    Forget = FUSE_FORGET,
    Getattr = FUSE_GETATTR,
    Setattr = FUSE_SETATTR,
    Mkdir = FUSE_MKDIR,
    Unlink = FUSE_UNLINK,
    Rmdir = FUSE_RMDIR,
    Rename = FUSE_RENAME,
    Open = FUSE_OPEN,
    Read = FUSE_READ,
    Write = FUSE_WRITE,
    Statfs = FUSE_STATFS,
    Release = FUSE_RELEASE,
    Fsync = FUSE_FSYNC,
    Setxattr = FUSE_SETXATTR,
    Getxattr = FUSE_GETXATTR,
    Listxattr = FUSE_LISTXATTR,
    Removexattr = FUSE_REMOVEXATTR,
    Flush = FUSE_FLUSH,
    Init = FUSE_INIT,
    Opendir = FUSE_OPENDIR,
    Readdir = FUSE_READDIR,
    Releasedir = FUSE_RELEASEDIR,
    Fsyncdir = FUSE_FSYNCDIR,
    Access = FUSE_ACCESS,
    Create = FUSE_CREATE,
    Destroy = FUSE_DESTROY,
    BatchForget = FUSE_BATCH_FORGET,
    Rename2 = FUSE_RENAME2,
}

impl FuseOpcode {
    /// Converts a raw wire opcode to [`FuseOpcode`], or `None` if unsupported.
    #[inline]
    pub const fn from_u32(val: u32) -> Option<Self> {
        match val {
            FUSE_LOOKUP => Some(Self::Lookup),
            FUSE_FORGET => Some(Self::Forget),
            FUSE_GETATTR => Some(Self::Getattr),
            FUSE_SETATTR => Some(Self::Setattr),
            FUSE_MKDIR => Some(Self::Mkdir),
            FUSE_UNLINK => Some(Self::Unlink),
            FUSE_RMDIR => Some(Self::Rmdir),
            FUSE_RENAME => Some(Self::Rename),
            FUSE_OPEN => Some(Self::Open),
            FUSE_READ => Some(Self::Read),
            FUSE_WRITE => Some(Self::Write),
            FUSE_STATFS => Some(Self::Statfs),
            FUSE_RELEASE => Some(Self::Release),
            FUSE_FSYNC => Some(Self::Fsync),
            FUSE_SETXATTR => Some(Self::Setxattr),
            FUSE_GETXATTR => Some(Self::Getxattr),
            FUSE_LISTXATTR => Some(Self::Listxattr),
            FUSE_REMOVEXATTR => Some(Self::Removexattr),
            FUSE_FLUSH => Some(Self::Flush),
            FUSE_INIT => Some(Self::Init),
            FUSE_OPENDIR => Some(Self::Opendir),
            FUSE_READDIR => Some(Self::Readdir),
            FUSE_RELEASEDIR => Some(Self::Releasedir),
            FUSE_FSYNCDIR => Some(Self::Fsyncdir),
            FUSE_ACCESS => Some(Self::Access),
            FUSE_CREATE => Some(Self::Create),
            FUSE_DESTROY => Some(Self::Destroy),
            FUSE_BATCH_FORGET => Some(Self::BatchForget),
            FUSE_RENAME2 => Some(Self::Rename2),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Bijective NodeID <-> OIFS Inode ID mapping & dirent alignment
// ---------------------------------------------------------------------------

/// Maps an OIFS inode ID (`0` = root) to a FUSE node ID (`1` = `FUSE_ROOT_ID`).
///
/// Returns `None` only on `u64::MAX` overflow.
#[inline]
pub const fn inode_to_nodeid(inode_id: u64) -> Option<u64> {
    inode_id.checked_add(1)
}

/// Maps a FUSE node ID (`1` = `FUSE_ROOT_ID`) back to an OIFS inode ID (`0` = root).
///
/// Returns `None` for the reserved invalid FUSE node ID `0`.
#[inline]
pub const fn nodeid_to_inode(nodeid: u64) -> Option<u64> {
    if nodeid == 0 { None } else { Some(nodeid - 1) }
}

/// Rounds a `fuse_dirent` record byte length up to an 8-byte alignment boundary.
///
/// Matches the kernel `FUSE_DIRENT_ALIGN(x)` macro: `(((x) + sizeof(u64) - 1) & ~(sizeof(u64) - 1))`.
#[inline]
pub const fn fuse_dirent_align(size: usize) -> usize {
    (size + 7) & !7
}

// ---------------------------------------------------------------------------
// macOS Finder metadata storm filter & pure-Rust UTF-8 NFD -> NFC normalization
// ---------------------------------------------------------------------------

/// Returns `true` if `name` is a macOS Finder / Spotlight / AppleDouble metadata filename
/// that would otherwise cause metadata storms and clutter inside an OIFS container.
#[inline]
pub fn is_macos_metadata_filename(name: &str) -> bool {
    name.starts_with("._")
        || matches!(
            name,
            ".DS_Store" | ".Spotlight-V100" | ".Trashes" | ".fseventsd" | ".TemporaryItems"
        )
}

/// Attempts canonical composition of `(base, combining)` for:
/// - Korean Hangul Jamo (`L + V -> LV`, `LV + T -> LVT`)
/// - Japanese Hiragana & Katakana dakuten (`U+3099`) and handakuten (`U+309A`)
/// - Latin, European, and Cyrillic combining diacritics
fn compose_pair(base: char, combining: char) -> Option<char> {
    let b = base as u32;
    let c = combining as u32;

    // 1. Hangul Jamo algorithmic composition (Unicode Standard §3.12).
    const S_BASE: u32 = 0xAC00;
    const L_BASE: u32 = 0x1100;
    const L_COUNT: u32 = 19;
    const V_BASE: u32 = 0x1161;
    const V_COUNT: u32 = 21;
    const T_BASE: u32 = 0x11A7;
    const T_COUNT: u32 = 28;
    const N_COUNT: u32 = V_COUNT * T_COUNT; // 588
    const S_COUNT: u32 = L_COUNT * N_COUNT; // 11172

    if (L_BASE..L_BASE + L_COUNT).contains(&b) && (V_BASE..V_BASE + V_COUNT).contains(&c) {
        let l_idx = b - L_BASE;
        let v_idx = c - V_BASE;
        return char::from_u32(S_BASE + (l_idx * V_COUNT + v_idx) * T_COUNT);
    }
    if (S_BASE..S_BASE + S_COUNT).contains(&b)
        && (b - S_BASE).is_multiple_of(T_COUNT)
        && (T_BASE + 1..T_BASE + T_COUNT).contains(&c)
    {
        let t_idx = c - T_BASE;
        return char::from_u32(b + t_idx);
    }

    // 2. Japanese Kana combining dakuten (U+3099) & handakuten (U+309A).
    if c == 0x3099 {
        // Hiragana dakuten
        if ((0x304B..=0x3061).contains(&b) && (b % 2 == 1))
            || ((0x3064..=0x3068).contains(&b) && b.is_multiple_of(2))
            || matches!(b, 0x306F | 0x3072 | 0x3075 | 0x3078 | 0x307B)
        {
            return char::from_u32(b + 1);
        }
        if b == 0x3046 {
            return Some('\u{3094}'); // う + ゛ -> ゔ
        }
        if b == 0x309D {
            return Some('\u{309E}'); // ゝ + ゛ -> ゞ
        }
        // Katakana dakuten
        if ((0x30AB..=0x30C1).contains(&b) && (b % 2 == 1))
            || ((0x30C4..=0x30C8).contains(&b) && b.is_multiple_of(2))
            || matches!(b, 0x30CF | 0x30D2 | 0x30D5 | 0x30D8 | 0x30DB)
        {
            return char::from_u32(b + 1);
        }
        return match b {
            0x30A6 => Some('\u{30F4}'), // ウ + ゛ -> ヴ
            0x30EF => Some('\u{30F7}'), // ワ + ゛ -> ヷ
            0x30F0 => Some('\u{30F8}'), // ヰ + ゛ -> ヸ
            0x30F1 => Some('\u{30F9}'), // ヱ + ゛ -> ヹ
            0x30F2 => Some('\u{30FA}'), // ヲ + ゛ -> ヺ
            0x30FD => Some('\u{30FE}'), // ヽ + ゛ -> ヾ
            _ => None,
        };
    }
    if c == 0x309A {
        if matches!(
            b,
            0x306F | 0x3072 | 0x3075 | 0x3078 | 0x307B | 0x30CF | 0x30D2 | 0x30D5 | 0x30D8 | 0x30DB
        ) {
            return char::from_u32(b + 2);
        }
        return None;
    }

    // 3. Latin, European, and Cyrillic combining diacritical marks.
    match (base, combining) {
        // U+0300 COMBINING GRAVE ACCENT
        ('A', '\u{0300}') => Some('\u{00C0}'),
        ('E', '\u{0300}') => Some('\u{00C8}'),
        ('I', '\u{0300}') => Some('\u{00CC}'),
        ('O', '\u{0300}') => Some('\u{00D2}'),
        ('U', '\u{0300}') => Some('\u{00D9}'),
        ('a', '\u{0300}') => Some('\u{00E0}'),
        ('e', '\u{0300}') => Some('\u{00E8}'),
        ('i', '\u{0300}') => Some('\u{00EC}'),
        ('o', '\u{0300}') => Some('\u{00F2}'),
        ('u', '\u{0300}') => Some('\u{00F9}'),
        ('N', '\u{0300}') => Some('\u{01F8}'),
        ('n', '\u{0300}') => Some('\u{01F9}'),
        ('W', '\u{0300}') => Some('\u{1E80}'),
        ('w', '\u{0300}') => Some('\u{1E81}'),
        ('Y', '\u{0300}') => Some('\u{1EF2}'),
        ('y', '\u{0300}') => Some('\u{1EF3}'),
        // U+0301 COMBINING ACUTE ACCENT
        ('A', '\u{0301}') => Some('\u{00C1}'),
        ('E', '\u{0301}') => Some('\u{00C9}'),
        ('I', '\u{0301}') => Some('\u{00CD}'),
        ('O', '\u{0301}') => Some('\u{00D3}'),
        ('U', '\u{0301}') => Some('\u{00DA}'),
        ('Y', '\u{0301}') => Some('\u{00DD}'),
        ('a', '\u{0301}') => Some('\u{00E1}'),
        ('e', '\u{0301}') => Some('\u{00E9}'),
        ('i', '\u{0301}') => Some('\u{00ED}'),
        ('o', '\u{0301}') => Some('\u{00F3}'),
        ('u', '\u{0301}') => Some('\u{00FA}'),
        ('y', '\u{0301}') => Some('\u{00FD}'),
        ('C', '\u{0301}') => Some('\u{0106}'),
        ('c', '\u{0301}') => Some('\u{0107}'),
        ('L', '\u{0301}') => Some('\u{0139}'),
        ('l', '\u{0301}') => Some('\u{013A}'),
        ('N', '\u{0301}') => Some('\u{0143}'),
        ('n', '\u{0301}') => Some('\u{0144}'),
        ('R', '\u{0301}') => Some('\u{0154}'),
        ('r', '\u{0301}') => Some('\u{0155}'),
        ('S', '\u{0301}') => Some('\u{015A}'),
        ('s', '\u{0301}') => Some('\u{015B}'),
        ('Z', '\u{0301}') => Some('\u{0179}'),
        ('z', '\u{0301}') => Some('\u{017A}'),
        // U+0302 COMBINING CIRCUMFLEX ACCENT
        ('A', '\u{0302}') => Some('\u{00C2}'),
        ('E', '\u{0302}') => Some('\u{00CA}'),
        ('I', '\u{0302}') => Some('\u{00CE}'),
        ('O', '\u{0302}') => Some('\u{00D4}'),
        ('U', '\u{0302}') => Some('\u{00DB}'),
        ('a', '\u{0302}') => Some('\u{00E2}'),
        ('e', '\u{0302}') => Some('\u{00EA}'),
        ('i', '\u{0302}') => Some('\u{00EE}'),
        ('o', '\u{0302}') => Some('\u{00F4}'),
        ('u', '\u{0302}') => Some('\u{00FB}'),
        ('C', '\u{0302}') => Some('\u{0108}'),
        ('c', '\u{0302}') => Some('\u{0109}'),
        ('G', '\u{0302}') => Some('\u{011C}'),
        ('g', '\u{0302}') => Some('\u{011D}'),
        ('H', '\u{0302}') => Some('\u{0124}'),
        ('h', '\u{0302}') => Some('\u{0125}'),
        ('J', '\u{0302}') => Some('\u{0134}'),
        ('j', '\u{0302}') => Some('\u{0135}'),
        ('S', '\u{0302}') => Some('\u{015C}'),
        ('s', '\u{0302}') => Some('\u{015D}'),
        ('W', '\u{0302}') => Some('\u{0174}'),
        ('w', '\u{0302}') => Some('\u{0175}'),
        ('Y', '\u{0302}') => Some('\u{0176}'),
        ('y', '\u{0302}') => Some('\u{0177}'),
        // U+0303 COMBINING TILDE
        ('A', '\u{0303}') => Some('\u{00C3}'),
        ('N', '\u{0303}') => Some('\u{00D1}'),
        ('O', '\u{0303}') => Some('\u{00D5}'),
        ('a', '\u{0303}') => Some('\u{00E3}'),
        ('n', '\u{0303}') => Some('\u{00F1}'),
        ('o', '\u{0303}') => Some('\u{00F5}'),
        ('I', '\u{0303}') => Some('\u{0128}'),
        ('i', '\u{0303}') => Some('\u{0129}'),
        ('U', '\u{0303}') => Some('\u{0168}'),
        ('u', '\u{0303}') => Some('\u{0169}'),
        // U+0304 COMBINING MACRON
        ('A', '\u{0304}') => Some('\u{0100}'),
        ('a', '\u{0304}') => Some('\u{0101}'),
        ('E', '\u{0304}') => Some('\u{0112}'),
        ('e', '\u{0304}') => Some('\u{0113}'),
        ('I', '\u{0304}') => Some('\u{012A}'),
        ('i', '\u{0304}') => Some('\u{012B}'),
        ('O', '\u{0304}') => Some('\u{014C}'),
        ('o', '\u{0304}') => Some('\u{014D}'),
        ('U', '\u{0304}') => Some('\u{016A}'),
        ('u', '\u{0304}') => Some('\u{016B}'),
        // U+0306 COMBINING BREVE
        ('A', '\u{0306}') => Some('\u{0102}'),
        ('a', '\u{0306}') => Some('\u{0103}'),
        ('E', '\u{0306}') => Some('\u{0114}'),
        ('e', '\u{0306}') => Some('\u{0115}'),
        ('G', '\u{0306}') => Some('\u{011E}'),
        ('g', '\u{0306}') => Some('\u{011F}'),
        ('I', '\u{0306}') => Some('\u{012C}'),
        ('i', '\u{0306}') => Some('\u{012D}'),
        ('O', '\u{0306}') => Some('\u{014E}'),
        ('o', '\u{0306}') => Some('\u{014F}'),
        ('U', '\u{0306}') => Some('\u{016C}'),
        ('u', '\u{0306}') => Some('\u{016D}'),
        ('\u{0418}', '\u{0306}') => Some('\u{0419}'), // И + ̆ -> Й
        ('\u{0438}', '\u{0306}') => Some('\u{0439}'), // и + ̆ -> й
        // U+0307 COMBINING DOT ABOVE
        ('C', '\u{0307}') => Some('\u{010A}'),
        ('c', '\u{0307}') => Some('\u{010B}'),
        ('E', '\u{0307}') => Some('\u{0116}'),
        ('e', '\u{0307}') => Some('\u{0117}'),
        ('G', '\u{0307}') => Some('\u{0120}'),
        ('g', '\u{0307}') => Some('\u{0121}'),
        ('I', '\u{0307}') => Some('\u{0130}'),
        ('Z', '\u{0307}') => Some('\u{017B}'),
        ('z', '\u{0307}') => Some('\u{017C}'),
        // U+0308 COMBINING DIAERESIS
        ('A', '\u{0308}') => Some('\u{00C4}'),
        ('E', '\u{0308}') => Some('\u{00CB}'),
        ('I', '\u{0308}') => Some('\u{00CF}'),
        ('O', '\u{0308}') => Some('\u{00D6}'),
        ('U', '\u{0308}') => Some('\u{00DC}'),
        ('Y', '\u{0308}') => Some('\u{0178}'),
        ('a', '\u{0308}') => Some('\u{00E4}'),
        ('e', '\u{0308}') => Some('\u{00EB}'),
        ('i', '\u{0308}') => Some('\u{00EF}'),
        ('o', '\u{0308}') => Some('\u{00F6}'),
        ('u', '\u{0308}') => Some('\u{00FC}'),
        ('y', '\u{0308}') => Some('\u{00FF}'),
        ('\u{0415}', '\u{0308}') => Some('\u{0401}'), // Е + ̈ -> Ё
        ('\u{0435}', '\u{0308}') => Some('\u{0451}'), // е + ̈ -> ё
        // U+030A COMBINING RING ABOVE
        ('A', '\u{030A}') => Some('\u{00C5}'),
        ('a', '\u{030A}') => Some('\u{00E5}'),
        ('U', '\u{030A}') => Some('\u{016E}'),
        ('u', '\u{030A}') => Some('\u{016F}'),
        // U+030B COMBINING DOUBLE ACUTE ACCENT
        ('O', '\u{030B}') => Some('\u{0150}'),
        ('o', '\u{030B}') => Some('\u{0151}'),
        ('U', '\u{030B}') => Some('\u{0170}'),
        ('u', '\u{030B}') => Some('\u{0171}'),
        // U+030C COMBINING CARON
        ('C', '\u{030C}') => Some('\u{010C}'),
        ('c', '\u{030C}') => Some('\u{010D}'),
        ('D', '\u{030C}') => Some('\u{010E}'),
        ('d', '\u{030C}') => Some('\u{010F}'),
        ('E', '\u{030C}') => Some('\u{011A}'),
        ('e', '\u{030C}') => Some('\u{011B}'),
        ('L', '\u{030C}') => Some('\u{013D}'),
        ('l', '\u{030C}') => Some('\u{013E}'),
        ('N', '\u{030C}') => Some('\u{0147}'),
        ('n', '\u{030C}') => Some('\u{0148}'),
        ('R', '\u{030C}') => Some('\u{0158}'),
        ('r', '\u{030C}') => Some('\u{0159}'),
        ('S', '\u{030C}') => Some('\u{0160}'),
        ('s', '\u{030C}') => Some('\u{0161}'),
        ('T', '\u{030C}') => Some('\u{0164}'),
        ('t', '\u{030C}') => Some('\u{0165}'),
        ('Z', '\u{030C}') => Some('\u{017D}'),
        ('z', '\u{030C}') => Some('\u{017E}'),
        // U+0327 COMBINING CEDILLA
        ('C', '\u{0327}') => Some('\u{00C7}'),
        ('c', '\u{0327}') => Some('\u{00E7}'),
        ('G', '\u{0327}') => Some('\u{0122}'),
        ('g', '\u{0327}') => Some('\u{0123}'),
        ('K', '\u{0327}') => Some('\u{0136}'),
        ('k', '\u{0327}') => Some('\u{0137}'),
        ('L', '\u{0327}') => Some('\u{013B}'),
        ('l', '\u{0327}') => Some('\u{013C}'),
        ('N', '\u{0327}') => Some('\u{0145}'),
        ('n', '\u{0327}') => Some('\u{0146}'),
        ('R', '\u{0327}') => Some('\u{0156}'),
        ('r', '\u{0327}') => Some('\u{0157}'),
        ('S', '\u{0327}') => Some('\u{015E}'),
        ('s', '\u{0327}') => Some('\u{015F}'),
        ('T', '\u{0327}') => Some('\u{0162}'),
        ('t', '\u{0327}') => Some('\u{0163}'),
        // U+0328 COMBINING OGONEK
        ('A', '\u{0328}') => Some('\u{0104}'),
        ('a', '\u{0328}') => Some('\u{0105}'),
        ('E', '\u{0328}') => Some('\u{0118}'),
        ('e', '\u{0328}') => Some('\u{0119}'),
        ('I', '\u{0328}') => Some('\u{012E}'),
        ('i', '\u{0328}') => Some('\u{012F}'),
        ('U', '\u{0328}') => Some('\u{0172}'),
        ('u', '\u{0328}') => Some('\u{0173}'),
        _ => None,
    }
}

/// Normalizes a UTF-8 filename from NFD (as emitted by macOS APFS / Finder) into NFC
/// (as expected by Linux and OIFS directory indices).
///
/// Features a zero-allocation fast path for ASCII strings and already-composed UTF-8
/// strings, plus canonical composition for Latin diacritics, Japanese Kana dakuten /
/// handakuten, and Korean Hangul Jamo (`L + V + T`).
pub fn normalize_filename_nfc(name: &str) -> Cow<'_, str> {
    if name.is_ascii() {
        return Cow::Borrowed(name);
    }

    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Cow::Borrowed(name);
    };

    // Quick scan to avoid allocating when no composable character is present.
    let mut prev = first;
    let mut needs_compose = false;
    for ch in chars {
        if compose_pair(prev, ch).is_some() {
            needs_compose = true;
            break;
        }
        prev = ch;
    }
    if !needs_compose {
        return Cow::Borrowed(name);
    }

    let mut out = String::with_capacity(name.len());
    let mut last: Option<char> = None;
    for ch in name.chars() {
        if let Some(prev_ch) = last
            && let Some(composed) = compose_pair(prev_ch, ch)
        {
            last = Some(composed);
            continue;
        }
        if let Some(prev_ch) = last {
            out.push(prev_ch);
        }
        last = Some(ch);
    }
    if let Some(prev_ch) = last {
        out.push(prev_ch);
    }
    Cow::Owned(out)
}

// ---------------------------------------------------------------------------
// Little-endian wire reader/writer helpers
// ---------------------------------------------------------------------------

#[inline]
fn read_u16_le(buf: &[u8], off: usize) -> Option<u16> {
    let slice = buf.get(off..off.checked_add(2)?)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

#[inline]
fn read_u32_le(buf: &[u8], off: usize) -> Option<u32> {
    let slice = buf.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

#[inline]
fn read_i32_le(buf: &[u8], off: usize) -> Option<i32> {
    let slice = buf.get(off..off.checked_add(4)?)?;
    Some(i32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

#[inline]
fn read_u64_le(buf: &[u8], off: usize) -> Option<u64> {
    let slice = buf.get(off..off.checked_add(8)?)?;
    Some(u64::from_le_bytes([
        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
    ]))
}

// ---------------------------------------------------------------------------
// FUSE Kernel ABI v7.28 wire structures
// ---------------------------------------------------------------------------

/// 40-byte request header (`struct fuse_in_header`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseInHeader {
    pub len: u32,
    pub opcode: u32,
    pub unique: u64,
    pub nodeid: u64,
    pub uid: u32,
    pub gid: u32,
    pub pid: u32,
    pub padding: u32,
}

impl FuseInHeader {
    /// Parses a complete FUSE request frame header and validates that `buf.len() >= 40`
    /// and `header.len as usize == buf.len()`.
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < FUSE_IN_HEADER_SIZE {
            return None;
        }
        let header = Self::parse_prefix(buf)?;
        if (header.len as usize) != buf.len() {
            return None;
        }
        Some(header)
    }

    /// Parses only the 40-byte header prefix without requiring the full payload to be
    /// present in `buf` yet (used when reading framed streams). Rejects `len < 40`.
    pub fn parse_prefix(buf: &[u8]) -> Option<Self> {
        if buf.len() < FUSE_IN_HEADER_SIZE {
            return None;
        }
        let len = read_u32_le(buf, 0)?;
        if (len as usize) < FUSE_IN_HEADER_SIZE {
            return None;
        }
        Some(Self {
            len,
            opcode: read_u32_le(buf, 4)?,
            unique: read_u64_le(buf, 8)?,
            nodeid: read_u64_le(buf, 16)?,
            uid: read_u32_le(buf, 24)?,
            gid: read_u32_le(buf, 28)?,
            pid: read_u32_le(buf, 32)?,
            padding: read_u32_le(buf, 36)?,
        })
    }

    /// Encodes the 40-byte header into little-endian bytes.
    pub fn to_bytes(&self) -> [u8; FUSE_IN_HEADER_SIZE] {
        let mut out = [0u8; FUSE_IN_HEADER_SIZE];
        out[0..4].copy_from_slice(&self.len.to_le_bytes());
        out[4..8].copy_from_slice(&self.opcode.to_le_bytes());
        out[8..16].copy_from_slice(&self.unique.to_le_bytes());
        out[16..24].copy_from_slice(&self.nodeid.to_le_bytes());
        out[24..28].copy_from_slice(&self.uid.to_le_bytes());
        out[28..32].copy_from_slice(&self.gid.to_le_bytes());
        out[32..36].copy_from_slice(&self.pid.to_le_bytes());
        out[36..40].copy_from_slice(&self.padding.to_le_bytes());
        out
    }

    /// Builds a complete little-endian request frame (`FuseInHeader` + `payload`).
    pub fn build_frame(opcode: u32, unique: u64, nodeid: u64, payload: &[u8]) -> Vec<u8> {
        let total_len = FUSE_IN_HEADER_SIZE
            .checked_add(payload.len())
            .and_then(|n| u32::try_from(n).ok())
            .expect("FUSE request frame length exceeds u32::MAX");
        let hdr = Self {
            len: total_len,
            opcode,
            unique,
            nodeid,
            uid: 0,
            gid: 0,
            pid: 0,
            padding: 0,
        };
        let mut frame = Vec::with_capacity(total_len as usize);
        frame.extend_from_slice(&hdr.to_bytes());
        frame.extend_from_slice(payload);
        frame
    }
}

/// 16-byte response header (`struct fuse_out_header`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseOutHeader {
    pub len: u32,
    pub error: i32,
    pub unique: u64,
}

impl FuseOutHeader {
    /// Parses a 16-byte response header from `buf`.
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < FUSE_OUT_HEADER_SIZE {
            return None;
        }
        Some(Self {
            len: read_u32_le(buf, 0)?,
            error: read_i32_le(buf, 4)?,
            unique: read_u64_le(buf, 8)?,
        })
    }

    /// Encodes the 16-byte header into little-endian bytes.
    pub fn to_bytes(&self) -> [u8; FUSE_OUT_HEADER_SIZE] {
        let mut out = [0u8; FUSE_OUT_HEADER_SIZE];
        out[0..4].copy_from_slice(&self.len.to_le_bytes());
        out[4..8].copy_from_slice(&self.error.to_le_bytes());
        out[8..16].copy_from_slice(&self.unique.to_le_bytes());
        out
    }

    /// Builds a 16-byte error response frame (`error` must be negative, e.g. `-libc::ENOENT`).
    pub fn error_frame(unique: u64, errno: i32) -> Vec<u8> {
        let neg_err = if errno > 0 { -errno } else { errno };
        let hdr = Self {
            len: FUSE_OUT_HEADER_SIZE as u32,
            error: neg_err,
            unique,
        };
        hdr.to_bytes().to_vec()
    }

    /// Builds a response frame with `error = 0` and an optional payload body.
    pub fn ok_frame(unique: u64, body: &[u8]) -> Vec<u8> {
        let total = match FUSE_OUT_HEADER_SIZE
            .checked_add(body.len())
            .and_then(|n| u32::try_from(n).ok())
        {
            Some(n) => n,
            None => return Self::error_frame(unique, -libc::EOVERFLOW),
        };
        let hdr = Self {
            len: total,
            error: 0,
            unique,
        };
        let mut out = Vec::with_capacity(total as usize);
        out.extend_from_slice(&hdr.to_bytes());
        out.extend_from_slice(body);
        out
    }
}

/// Inode attribute record (`struct fuse_attr`): 88 bytes on Linux ABI, 104 bytes on macOS ABI.
///
/// The Darwin/macFUSE extension fields (`crtime`, `crtimensec`, `flags`) are serialized only
/// when encoding with [`FuseAbiPlatform::MacOs`] (`to_macos_bytes` / `to_bytes_for`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseAttr {
    pub ino: u64,
    pub size: u64,
    pub blocks: u64,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    /// Creation/birth time seconds (macOS/Darwin ABI extension; omitted in Linux 88B wire format).
    pub crtime: u64,
    pub atimensec: u32,
    pub mtimensec: u32,
    pub ctimensec: u32,
    /// Creation/birth time nanoseconds (macOS/Darwin ABI extension).
    pub crtimensec: u32,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u32,
    /// BSD file flags (`chflags`, macOS/Darwin ABI extension).
    pub flags: u32,
    pub blksize: u32,
    pub padding: u32,
}

impl FuseAttr {
    /// Parses a standard 88-byte Linux `struct fuse_attr`.
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < FUSE_ATTR_SIZE {
            return None;
        }
        Some(Self {
            ino: read_u64_le(buf, 0)?,
            size: read_u64_le(buf, 8)?,
            blocks: read_u64_le(buf, 16)?,
            atime: read_u64_le(buf, 24)?,
            mtime: read_u64_le(buf, 32)?,
            ctime: read_u64_le(buf, 40)?,
            crtime: 0,
            atimensec: read_u32_le(buf, 48)?,
            mtimensec: read_u32_le(buf, 52)?,
            ctimensec: read_u32_le(buf, 56)?,
            crtimensec: 0,
            mode: read_u32_le(buf, 60)?,
            nlink: read_u32_le(buf, 64)?,
            uid: read_u32_le(buf, 68)?,
            gid: read_u32_le(buf, 72)?,
            rdev: read_u32_le(buf, 76)?,
            flags: 0,
            blksize: read_u32_le(buf, 80)?,
            padding: read_u32_le(buf, 84)?,
        })
    }

    /// Parses a 104-byte macOS/Darwin (macFUSE / osxfuse) `struct fuse_attr`.
    pub fn parse_macos(buf: &[u8]) -> Option<Self> {
        if buf.len() < FUSE_ATTR_MACOS_SIZE {
            return None;
        }
        Some(Self {
            ino: read_u64_le(buf, 0)?,
            size: read_u64_le(buf, 8)?,
            blocks: read_u64_le(buf, 16)?,
            atime: read_u64_le(buf, 24)?,
            mtime: read_u64_le(buf, 32)?,
            ctime: read_u64_le(buf, 40)?,
            crtime: read_u64_le(buf, 48)?,
            atimensec: read_u32_le(buf, 56)?,
            mtimensec: read_u32_le(buf, 60)?,
            ctimensec: read_u32_le(buf, 64)?,
            crtimensec: read_u32_le(buf, 68)?,
            mode: read_u32_le(buf, 72)?,
            nlink: read_u32_le(buf, 76)?,
            uid: read_u32_le(buf, 80)?,
            gid: read_u32_le(buf, 84)?,
            rdev: read_u32_le(buf, 88)?,
            flags: read_u32_le(buf, 92)?,
            blksize: read_u32_le(buf, 96)?,
            padding: read_u32_le(buf, 100)?,
        })
    }

    /// Parses a `FuseAttr` according to `platform`.
    pub fn parse_for(buf: &[u8], platform: FuseAbiPlatform) -> Option<Self> {
        match platform {
            FuseAbiPlatform::Linux => Self::parse(buf),
            FuseAbiPlatform::MacOs => Self::parse_macos(buf),
        }
    }

    /// Encodes as a standard 88-byte Linux `struct fuse_attr`.
    pub fn to_bytes(&self) -> [u8; FUSE_ATTR_SIZE] {
        let mut out = [0u8; FUSE_ATTR_SIZE];
        out[0..8].copy_from_slice(&self.ino.to_le_bytes());
        out[8..16].copy_from_slice(&self.size.to_le_bytes());
        out[16..24].copy_from_slice(&self.blocks.to_le_bytes());
        out[24..32].copy_from_slice(&self.atime.to_le_bytes());
        out[32..40].copy_from_slice(&self.mtime.to_le_bytes());
        out[40..48].copy_from_slice(&self.ctime.to_le_bytes());
        out[48..52].copy_from_slice(&self.atimensec.to_le_bytes());
        out[52..56].copy_from_slice(&self.mtimensec.to_le_bytes());
        out[56..60].copy_from_slice(&self.ctimensec.to_le_bytes());
        out[60..64].copy_from_slice(&self.mode.to_le_bytes());
        out[64..68].copy_from_slice(&self.nlink.to_le_bytes());
        out[68..72].copy_from_slice(&self.uid.to_le_bytes());
        out[72..76].copy_from_slice(&self.gid.to_le_bytes());
        out[76..80].copy_from_slice(&self.rdev.to_le_bytes());
        out[80..84].copy_from_slice(&self.blksize.to_le_bytes());
        out[84..88].copy_from_slice(&self.padding.to_le_bytes());
        out
    }

    /// Encodes as a 104-byte macOS/Darwin (macFUSE / osxfuse) `struct fuse_attr`.
    pub fn to_macos_bytes(&self) -> [u8; FUSE_ATTR_MACOS_SIZE] {
        let mut out = [0u8; FUSE_ATTR_MACOS_SIZE];
        out[0..8].copy_from_slice(&self.ino.to_le_bytes());
        out[8..16].copy_from_slice(&self.size.to_le_bytes());
        out[16..24].copy_from_slice(&self.blocks.to_le_bytes());
        out[24..32].copy_from_slice(&self.atime.to_le_bytes());
        out[32..40].copy_from_slice(&self.mtime.to_le_bytes());
        out[40..48].copy_from_slice(&self.ctime.to_le_bytes());
        out[48..56].copy_from_slice(&self.crtime.to_le_bytes());
        out[56..60].copy_from_slice(&self.atimensec.to_le_bytes());
        out[60..64].copy_from_slice(&self.mtimensec.to_le_bytes());
        out[64..68].copy_from_slice(&self.ctimensec.to_le_bytes());
        out[68..72].copy_from_slice(&self.crtimensec.to_le_bytes());
        out[72..76].copy_from_slice(&self.mode.to_le_bytes());
        out[76..80].copy_from_slice(&self.nlink.to_le_bytes());
        out[80..84].copy_from_slice(&self.uid.to_le_bytes());
        out[84..88].copy_from_slice(&self.gid.to_le_bytes());
        out[88..92].copy_from_slice(&self.rdev.to_le_bytes());
        out[92..96].copy_from_slice(&self.flags.to_le_bytes());
        out[96..100].copy_from_slice(&self.blksize.to_le_bytes());
        out[100..104].copy_from_slice(&self.padding.to_le_bytes());
        out
    }

    /// Encodes as little-endian bytes for the requested [`FuseAbiPlatform`].
    pub fn to_bytes_for(&self, platform: FuseAbiPlatform) -> Vec<u8> {
        match platform {
            FuseAbiPlatform::Linux => self.to_bytes().to_vec(),
            FuseAbiPlatform::MacOs => self.to_macos_bytes().to_vec(),
        }
    }

    /// Builds a [`FuseAttr`] from an OIFS [`Inode`] and FUSE `nodeid`.
    pub fn from_inode(
        nodeid: u64,
        inode: &Inode,
        opts: &FuseMountOptions,
        read_only: bool,
    ) -> Self {
        let (mode, nlink) = match inode.mode {
            FileType::Directory => {
                let perm = if read_only { 0o555 } else { 0o755 };
                (POSIX_S_IFDIR | perm, 2)
            }
            FileType::File => {
                let perm = if read_only { 0o444 } else { 0o644 };
                (POSIX_S_IFREG | perm, 1)
            }
        };
        let logical_size = logical_file_size(inode);
        let physical_bytes = if inode.compressed_size > 0 {
            inode.compressed_size
        } else {
            inode.size
        };
        let blocks = physical_bytes.div_ceil(512);
        let ctime = if inode.created_at != 0 {
            inode.created_at
        } else {
            inode.modified_at
        };
        Self {
            ino: nodeid,
            size: logical_size,
            blocks,
            atime: inode.modified_at,
            mtime: inode.modified_at,
            ctime,
            crtime: inode.created_at,
            atimensec: 0,
            mtimensec: 0,
            ctimensec: 0,
            crtimensec: 0,
            mode,
            nlink,
            uid: opts.uid,
            gid: opts.gid,
            rdev: 0,
            flags: 0,
            blksize: BLOCK_SIZE as u32,
            padding: 0,
        }
    }
}

/// Lookup/mkdir/create response (`struct fuse_entry_out`: 128B Linux, 144B macOS).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseEntryOut {
    pub nodeid: u64,
    pub generation: u64,
    pub entry_valid: u64,
    pub attr_valid: u64,
    pub entry_valid_nsec: u32,
    pub attr_valid_nsec: u32,
    pub attr: FuseAttr,
}

impl FuseEntryOut {
    /// Parses a standard 128-byte Linux `struct fuse_entry_out`.
    pub fn parse(buf: &[u8]) -> Option<Self> {
        Self::parse_for(buf, FuseAbiPlatform::Linux)
    }

    /// Parses a 144-byte macOS/Darwin `struct fuse_entry_out`.
    pub fn parse_macos(buf: &[u8]) -> Option<Self> {
        Self::parse_for(buf, FuseAbiPlatform::MacOs)
    }

    /// Parses a `struct fuse_entry_out` for `platform`.
    pub fn parse_for(buf: &[u8], platform: FuseAbiPlatform) -> Option<Self> {
        if buf.len() < platform.entry_out_size() {
            return None;
        }
        Some(Self {
            nodeid: read_u64_le(buf, 0)?,
            generation: read_u64_le(buf, 8)?,
            entry_valid: read_u64_le(buf, 16)?,
            attr_valid: read_u64_le(buf, 24)?,
            entry_valid_nsec: read_u32_le(buf, 32)?,
            attr_valid_nsec: read_u32_le(buf, 36)?,
            attr: FuseAttr::parse_for(&buf[40..], platform)?,
        })
    }

    /// Encodes as a standard 128-byte Linux `struct fuse_entry_out`.
    pub fn to_bytes(&self) -> [u8; FUSE_ENTRY_OUT_SIZE] {
        let mut out = [0u8; FUSE_ENTRY_OUT_SIZE];
        out[0..8].copy_from_slice(&self.nodeid.to_le_bytes());
        out[8..16].copy_from_slice(&self.generation.to_le_bytes());
        out[16..24].copy_from_slice(&self.entry_valid.to_le_bytes());
        out[24..32].copy_from_slice(&self.attr_valid.to_le_bytes());
        out[32..36].copy_from_slice(&self.entry_valid_nsec.to_le_bytes());
        out[36..40].copy_from_slice(&self.attr_valid_nsec.to_le_bytes());
        out[40..128].copy_from_slice(&self.attr.to_bytes());
        out
    }

    /// Encodes as a 144-byte macOS/Darwin `struct fuse_entry_out`.
    pub fn to_macos_bytes(&self) -> [u8; FUSE_ENTRY_OUT_MACOS_SIZE] {
        let mut out = [0u8; FUSE_ENTRY_OUT_MACOS_SIZE];
        out[0..8].copy_from_slice(&self.nodeid.to_le_bytes());
        out[8..16].copy_from_slice(&self.generation.to_le_bytes());
        out[16..24].copy_from_slice(&self.entry_valid.to_le_bytes());
        out[24..32].copy_from_slice(&self.attr_valid.to_le_bytes());
        out[32..36].copy_from_slice(&self.entry_valid_nsec.to_le_bytes());
        out[36..40].copy_from_slice(&self.attr_valid_nsec.to_le_bytes());
        out[40..144].copy_from_slice(&self.attr.to_macos_bytes());
        out
    }

    /// Encodes for `platform`.
    pub fn to_bytes_for(&self, platform: FuseAbiPlatform) -> Vec<u8> {
        match platform {
            FuseAbiPlatform::Linux => self.to_bytes().to_vec(),
            FuseAbiPlatform::MacOs => self.to_macos_bytes().to_vec(),
        }
    }
}

/// Getattr/setattr response (`struct fuse_attr_out`: 104B Linux, 120B macOS).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseAttrOut {
    pub attr_valid: u64,
    pub attr_valid_nsec: u32,
    pub dummy: u32,
    pub attr: FuseAttr,
}

impl FuseAttrOut {
    /// Parses a standard 104-byte Linux `struct fuse_attr_out`.
    pub fn parse(buf: &[u8]) -> Option<Self> {
        Self::parse_for(buf, FuseAbiPlatform::Linux)
    }

    /// Parses a 120-byte macOS/Darwin `struct fuse_attr_out`.
    pub fn parse_macos(buf: &[u8]) -> Option<Self> {
        Self::parse_for(buf, FuseAbiPlatform::MacOs)
    }

    /// Parses a `struct fuse_attr_out` for `platform`.
    pub fn parse_for(buf: &[u8], platform: FuseAbiPlatform) -> Option<Self> {
        if buf.len() < platform.attr_out_size() {
            return None;
        }
        Some(Self {
            attr_valid: read_u64_le(buf, 0)?,
            attr_valid_nsec: read_u32_le(buf, 8)?,
            dummy: read_u32_le(buf, 12)?,
            attr: FuseAttr::parse_for(&buf[16..], platform)?,
        })
    }

    /// Encodes as a standard 104-byte Linux `struct fuse_attr_out`.
    pub fn to_bytes(&self) -> [u8; FUSE_ATTR_OUT_SIZE] {
        let mut out = [0u8; FUSE_ATTR_OUT_SIZE];
        out[0..8].copy_from_slice(&self.attr_valid.to_le_bytes());
        out[8..12].copy_from_slice(&self.attr_valid_nsec.to_le_bytes());
        out[12..16].copy_from_slice(&self.dummy.to_le_bytes());
        out[16..104].copy_from_slice(&self.attr.to_bytes());
        out
    }

    /// Encodes as a 120-byte macOS/Darwin `struct fuse_attr_out`.
    pub fn to_macos_bytes(&self) -> [u8; FUSE_ATTR_OUT_MACOS_SIZE] {
        let mut out = [0u8; FUSE_ATTR_OUT_MACOS_SIZE];
        out[0..8].copy_from_slice(&self.attr_valid.to_le_bytes());
        out[8..12].copy_from_slice(&self.attr_valid_nsec.to_le_bytes());
        out[12..16].copy_from_slice(&self.dummy.to_le_bytes());
        out[16..120].copy_from_slice(&self.attr.to_macos_bytes());
        out
    }

    /// Encodes for `platform`.
    pub fn to_bytes_for(&self, platform: FuseAbiPlatform) -> Vec<u8> {
        match platform {
            FuseAbiPlatform::Linux => self.to_bytes().to_vec(),
            FuseAbiPlatform::MacOs => self.to_macos_bytes().to_vec(),
        }
    }
}

/// 8-byte open/opendir request payload (`struct fuse_open_in`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseOpenIn {
    pub flags: u32,
    pub unused: u32,
}

impl FuseOpenIn {
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < FUSE_OPEN_IN_SIZE {
            return None;
        }
        Some(Self {
            flags: read_u32_le(buf, 0)?,
            unused: read_u32_le(buf, 4)?,
        })
    }

    pub fn to_bytes(&self) -> [u8; FUSE_OPEN_IN_SIZE] {
        let mut out = [0u8; FUSE_OPEN_IN_SIZE];
        out[0..4].copy_from_slice(&self.flags.to_le_bytes());
        out[4..8].copy_from_slice(&self.unused.to_le_bytes());
        out
    }
}

/// 16-byte open/opendir/create reply payload (`struct fuse_open_out`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseOpenOut {
    pub fh: u64,
    pub open_flags: u32,
    pub padding: u32,
}

impl FuseOpenOut {
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < FUSE_OPEN_OUT_SIZE {
            return None;
        }
        Some(Self {
            fh: read_u64_le(buf, 0)?,
            open_flags: read_u32_le(buf, 8)?,
            padding: read_u32_le(buf, 12)?,
        })
    }

    pub fn to_bytes(&self) -> [u8; FUSE_OPEN_OUT_SIZE] {
        let mut out = [0u8; FUSE_OPEN_OUT_SIZE];
        out[0..8].copy_from_slice(&self.fh.to_le_bytes());
        out[8..12].copy_from_slice(&self.open_flags.to_le_bytes());
        out[12..16].copy_from_slice(&self.padding.to_le_bytes());
        out
    }
}

/// 40-byte read/readdir request payload (`struct fuse_read_in`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseReadIn {
    pub fh: u64,
    pub offset: u64,
    pub size: u32,
    pub read_flags: u32,
    pub lock_owner: u64,
    pub flags: u32,
    pub padding: u32,
}

impl FuseReadIn {
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < FUSE_READ_IN_SIZE {
            return None;
        }
        Some(Self {
            fh: read_u64_le(buf, 0)?,
            offset: read_u64_le(buf, 8)?,
            size: read_u32_le(buf, 16)?,
            read_flags: read_u32_le(buf, 20)?,
            lock_owner: read_u64_le(buf, 24)?,
            flags: read_u32_le(buf, 32)?,
            padding: read_u32_le(buf, 36)?,
        })
    }

    pub fn to_bytes(&self) -> [u8; FUSE_READ_IN_SIZE] {
        let mut out = [0u8; FUSE_READ_IN_SIZE];
        out[0..8].copy_from_slice(&self.fh.to_le_bytes());
        out[8..16].copy_from_slice(&self.offset.to_le_bytes());
        out[16..20].copy_from_slice(&self.size.to_le_bytes());
        out[20..24].copy_from_slice(&self.read_flags.to_le_bytes());
        out[24..32].copy_from_slice(&self.lock_owner.to_le_bytes());
        out[32..36].copy_from_slice(&self.flags.to_le_bytes());
        out[36..40].copy_from_slice(&self.padding.to_le_bytes());
        out
    }
}

/// 40-byte write request header (`struct fuse_write_in`), followed by `size` payload bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseWriteIn {
    pub fh: u64,
    pub offset: u64,
    pub size: u32,
    pub write_flags: u32,
    pub lock_owner: u64,
    pub flags: u32,
    pub padding: u32,
}

impl FuseWriteIn {
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < FUSE_WRITE_IN_SIZE {
            return None;
        }
        Some(Self {
            fh: read_u64_le(buf, 0)?,
            offset: read_u64_le(buf, 8)?,
            size: read_u32_le(buf, 16)?,
            write_flags: read_u32_le(buf, 20)?,
            lock_owner: read_u64_le(buf, 24)?,
            flags: read_u32_le(buf, 32)?,
            padding: read_u32_le(buf, 36)?,
        })
    }

    pub fn to_bytes(&self) -> [u8; FUSE_WRITE_IN_SIZE] {
        let mut out = [0u8; FUSE_WRITE_IN_SIZE];
        out[0..8].copy_from_slice(&self.fh.to_le_bytes());
        out[8..16].copy_from_slice(&self.offset.to_le_bytes());
        out[16..20].copy_from_slice(&self.size.to_le_bytes());
        out[20..24].copy_from_slice(&self.write_flags.to_le_bytes());
        out[24..32].copy_from_slice(&self.lock_owner.to_le_bytes());
        out[32..36].copy_from_slice(&self.flags.to_le_bytes());
        out[36..40].copy_from_slice(&self.padding.to_le_bytes());
        out
    }
}

/// 8-byte write response (`struct fuse_write_out`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseWriteOut {
    pub size: u32,
    pub padding: u32,
}

impl FuseWriteOut {
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < FUSE_WRITE_OUT_SIZE {
            return None;
        }
        Some(Self {
            size: read_u32_le(buf, 0)?,
            padding: read_u32_le(buf, 4)?,
        })
    }

    pub fn to_bytes(&self) -> [u8; FUSE_WRITE_OUT_SIZE] {
        let mut out = [0u8; FUSE_WRITE_OUT_SIZE];
        out[0..4].copy_from_slice(&self.size.to_le_bytes());
        out[4..8].copy_from_slice(&self.padding.to_le_bytes());
        out
    }
}

/// 16-byte create request header (`struct fuse_create_in`), followed by NUL-terminated name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseCreateIn {
    pub flags: u32,
    pub mode: u32,
    pub umask: u32,
    pub padding: u32,
}

impl FuseCreateIn {
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < FUSE_CREATE_IN_SIZE {
            return None;
        }
        Some(Self {
            flags: read_u32_le(buf, 0)?,
            mode: read_u32_le(buf, 4)?,
            umask: read_u32_le(buf, 8)?,
            padding: read_u32_le(buf, 12)?,
        })
    }

    pub fn to_bytes(&self) -> [u8; FUSE_CREATE_IN_SIZE] {
        let mut out = [0u8; FUSE_CREATE_IN_SIZE];
        out[0..4].copy_from_slice(&self.flags.to_le_bytes());
        out[4..8].copy_from_slice(&self.mode.to_le_bytes());
        out[8..12].copy_from_slice(&self.umask.to_le_bytes());
        out[12..16].copy_from_slice(&self.padding.to_le_bytes());
        out
    }
}

/// 8-byte mkdir request header (`struct fuse_mkdir_in`), followed by NUL-terminated name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseMkdirIn {
    pub mode: u32,
    pub umask: u32,
}

impl FuseMkdirIn {
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < FUSE_MKDIR_IN_SIZE {
            return None;
        }
        Some(Self {
            mode: read_u32_le(buf, 0)?,
            umask: read_u32_le(buf, 4)?,
        })
    }

    pub fn to_bytes(&self) -> [u8; FUSE_MKDIR_IN_SIZE] {
        let mut out = [0u8; FUSE_MKDIR_IN_SIZE];
        out[0..4].copy_from_slice(&self.mode.to_le_bytes());
        out[4..8].copy_from_slice(&self.umask.to_le_bytes());
        out
    }
}

/// 88-byte setattr request (`struct fuse_setattr_in`).
///
/// The leading fields (`valid`, `fh`, `size`, `lock_owner`, `atime`, `mtime`, `ctime`)
/// occupy identical byte offsets (`0..56`) on both Linux (88B) and macOS/macFUSE (128B).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseSetAttrIn {
    pub valid: u32,
    pub padding: u32,
    pub fh: u64,
    pub size: u64,
    pub lock_owner: u64,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub atimensec: u32,
    pub mtimensec: u32,
    pub ctimensec: u32,
    pub mode: u32,
    pub unused4: u32,
    pub uid: u32,
    pub gid: u32,
    pub unused5: u32,
}

impl FuseSetAttrIn {
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < FUSE_SETATTR_IN_SIZE {
            return None;
        }
        Some(Self {
            valid: read_u32_le(buf, 0)?,
            padding: read_u32_le(buf, 4)?,
            fh: read_u64_le(buf, 8)?,
            size: read_u64_le(buf, 16)?,
            lock_owner: read_u64_le(buf, 24)?,
            atime: read_u64_le(buf, 32)?,
            mtime: read_u64_le(buf, 40)?,
            ctime: read_u64_le(buf, 48)?,
            atimensec: read_u32_le(buf, 56)?,
            mtimensec: read_u32_le(buf, 60)?,
            ctimensec: read_u32_le(buf, 64)?,
            mode: read_u32_le(buf, 68)?,
            unused4: read_u32_le(buf, 72)?,
            uid: read_u32_le(buf, 76)?,
            gid: read_u32_le(buf, 80)?,
            unused5: read_u32_le(buf, 84)?,
        })
    }

    pub fn to_bytes(&self) -> [u8; FUSE_SETATTR_IN_SIZE] {
        let mut out = [0u8; FUSE_SETATTR_IN_SIZE];
        out[0..4].copy_from_slice(&self.valid.to_le_bytes());
        out[4..8].copy_from_slice(&self.padding.to_le_bytes());
        out[8..16].copy_from_slice(&self.fh.to_le_bytes());
        out[16..24].copy_from_slice(&self.size.to_le_bytes());
        out[24..32].copy_from_slice(&self.lock_owner.to_le_bytes());
        out[32..40].copy_from_slice(&self.atime.to_le_bytes());
        out[40..48].copy_from_slice(&self.mtime.to_le_bytes());
        out[48..56].copy_from_slice(&self.ctime.to_le_bytes());
        out[56..60].copy_from_slice(&self.atimensec.to_le_bytes());
        out[60..64].copy_from_slice(&self.mtimensec.to_le_bytes());
        out[64..68].copy_from_slice(&self.ctimensec.to_le_bytes());
        out[68..72].copy_from_slice(&self.mode.to_le_bytes());
        out[72..76].copy_from_slice(&self.unused4.to_le_bytes());
        out[76..80].copy_from_slice(&self.uid.to_le_bytes());
        out[80..84].copy_from_slice(&self.gid.to_le_bytes());
        out[84..88].copy_from_slice(&self.unused5.to_le_bytes());
        out
    }
}

/// 80-byte statfs payload (`struct fuse_kstatfs` / `struct fuse_statfs_out`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseKStatfs {
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
    pub bsize: u32,
    pub namelen: u32,
    pub frsize: u32,
    pub padding: u32,
    pub spare: [u32; 6],
}

impl FuseKStatfs {
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < FUSE_KSTATFS_SIZE {
            return None;
        }
        let mut spare = [0u32; 6];
        for (i, slot) in spare.iter_mut().enumerate() {
            *slot = read_u32_le(buf, 56 + i * 4)?;
        }
        Some(Self {
            blocks: read_u64_le(buf, 0)?,
            bfree: read_u64_le(buf, 8)?,
            bavail: read_u64_le(buf, 16)?,
            files: read_u64_le(buf, 24)?,
            ffree: read_u64_le(buf, 32)?,
            bsize: read_u32_le(buf, 40)?,
            namelen: read_u32_le(buf, 44)?,
            frsize: read_u32_le(buf, 48)?,
            padding: read_u32_le(buf, 52)?,
            spare,
        })
    }

    pub fn to_bytes(&self) -> [u8; FUSE_KSTATFS_SIZE] {
        let mut out = [0u8; FUSE_KSTATFS_SIZE];
        out[0..8].copy_from_slice(&self.blocks.to_le_bytes());
        out[8..16].copy_from_slice(&self.bfree.to_le_bytes());
        out[16..24].copy_from_slice(&self.bavail.to_le_bytes());
        out[24..32].copy_from_slice(&self.files.to_le_bytes());
        out[32..40].copy_from_slice(&self.ffree.to_le_bytes());
        out[40..44].copy_from_slice(&self.bsize.to_le_bytes());
        out[44..48].copy_from_slice(&self.namelen.to_le_bytes());
        out[48..52].copy_from_slice(&self.frsize.to_le_bytes());
        out[52..56].copy_from_slice(&self.padding.to_le_bytes());
        for (i, val) in self.spare.iter().enumerate() {
            let off = 56 + i * 4;
            out[off..off + 4].copy_from_slice(&val.to_le_bytes());
        }
        out
    }
}

/// 16-byte init handshake request (`struct fuse_init_in`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseInitIn {
    pub major: u32,
    pub minor: u32,
    pub max_readahead: u32,
    pub flags: u32,
}

impl FuseInitIn {
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < FUSE_INIT_IN_SIZE {
            return None;
        }
        Some(Self {
            major: read_u32_le(buf, 0)?,
            minor: read_u32_le(buf, 4)?,
            max_readahead: read_u32_le(buf, 8)?,
            flags: read_u32_le(buf, 12)?,
        })
    }

    pub fn to_bytes(&self) -> [u8; FUSE_INIT_IN_SIZE] {
        let mut out = [0u8; FUSE_INIT_IN_SIZE];
        out[0..4].copy_from_slice(&self.major.to_le_bytes());
        out[4..8].copy_from_slice(&self.minor.to_le_bytes());
        out[8..12].copy_from_slice(&self.max_readahead.to_le_bytes());
        out[12..16].copy_from_slice(&self.flags.to_le_bytes());
        out
    }
}

/// 64-byte init handshake reply (`struct fuse_init_out`, ABI v7.28).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseInitOut {
    pub major: u32,
    pub minor: u32,
    pub max_readahead: u32,
    pub flags: u32,
    pub max_background: u16,
    pub congestion_threshold: u16,
    pub max_write: u32,
    pub time_gran: u32,
    pub max_pages: u16,
    pub map_alignment: u16,
    pub unused: [u32; 8],
}

impl FuseInitOut {
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < FUSE_INIT_OUT_SIZE {
            return None;
        }
        let mut unused = [0u32; 8];
        for (i, slot) in unused.iter_mut().enumerate() {
            *slot = read_u32_le(buf, 32 + i * 4)?;
        }
        Some(Self {
            major: read_u32_le(buf, 0)?,
            minor: read_u32_le(buf, 4)?,
            max_readahead: read_u32_le(buf, 8)?,
            flags: read_u32_le(buf, 12)?,
            max_background: read_u16_le(buf, 16)?,
            congestion_threshold: read_u16_le(buf, 18)?,
            max_write: read_u32_le(buf, 20)?,
            time_gran: read_u32_le(buf, 24)?,
            max_pages: read_u16_le(buf, 28)?,
            map_alignment: read_u16_le(buf, 30)?,
            unused,
        })
    }

    pub fn to_bytes(&self) -> [u8; FUSE_INIT_OUT_SIZE] {
        let mut out = [0u8; FUSE_INIT_OUT_SIZE];
        out[0..4].copy_from_slice(&self.major.to_le_bytes());
        out[4..8].copy_from_slice(&self.minor.to_le_bytes());
        out[8..12].copy_from_slice(&self.max_readahead.to_le_bytes());
        out[12..16].copy_from_slice(&self.flags.to_le_bytes());
        out[16..18].copy_from_slice(&self.max_background.to_le_bytes());
        out[18..20].copy_from_slice(&self.congestion_threshold.to_le_bytes());
        out[20..24].copy_from_slice(&self.max_write.to_le_bytes());
        out[24..28].copy_from_slice(&self.time_gran.to_le_bytes());
        out[28..30].copy_from_slice(&self.max_pages.to_le_bytes());
        out[30..32].copy_from_slice(&self.map_alignment.to_le_bytes());
        for (i, val) in self.unused.iter().enumerate() {
            let off = 32 + i * 4;
            out[off..off + 4].copy_from_slice(&val.to_le_bytes());
        }
        out
    }
}

/// 24-byte release/releasedir request (`struct fuse_release_in`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseReleaseIn {
    pub fh: u64,
    pub flags: u32,
    pub release_flags: u32,
    pub lock_owner: u64,
}

impl FuseReleaseIn {
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < FUSE_RELEASE_IN_SIZE {
            return None;
        }
        Some(Self {
            fh: read_u64_le(buf, 0)?,
            flags: read_u32_le(buf, 8)?,
            release_flags: read_u32_le(buf, 12)?,
            lock_owner: read_u64_le(buf, 16)?,
        })
    }

    pub fn to_bytes(&self) -> [u8; FUSE_RELEASE_IN_SIZE] {
        let mut out = [0u8; FUSE_RELEASE_IN_SIZE];
        out[0..8].copy_from_slice(&self.fh.to_le_bytes());
        out[8..12].copy_from_slice(&self.flags.to_le_bytes());
        out[12..16].copy_from_slice(&self.release_flags.to_le_bytes());
        out[16..24].copy_from_slice(&self.lock_owner.to_le_bytes());
        out
    }
}

/// 24-byte flush request (`struct fuse_flush_in`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseFlushIn {
    pub fh: u64,
    pub unused: u32,
    pub padding: u32,
    pub lock_owner: u64,
}

impl FuseFlushIn {
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < FUSE_FLUSH_IN_SIZE {
            return None;
        }
        Some(Self {
            fh: read_u64_le(buf, 0)?,
            unused: read_u32_le(buf, 8)?,
            padding: read_u32_le(buf, 12)?,
            lock_owner: read_u64_le(buf, 16)?,
        })
    }

    pub fn to_bytes(&self) -> [u8; FUSE_FLUSH_IN_SIZE] {
        let mut out = [0u8; FUSE_FLUSH_IN_SIZE];
        out[0..8].copy_from_slice(&self.fh.to_le_bytes());
        out[8..12].copy_from_slice(&self.unused.to_le_bytes());
        out[12..16].copy_from_slice(&self.padding.to_le_bytes());
        out[16..24].copy_from_slice(&self.lock_owner.to_le_bytes());
        out
    }
}

/// 16-byte fsync/fsyncdir request (`struct fuse_fsync_in`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseFsyncIn {
    pub fh: u64,
    pub fsync_flags: u32,
    pub padding: u32,
}

impl FuseFsyncIn {
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < FUSE_FSYNC_IN_SIZE {
            return None;
        }
        Some(Self {
            fh: read_u64_le(buf, 0)?,
            fsync_flags: read_u32_le(buf, 8)?,
            padding: read_u32_le(buf, 12)?,
        })
    }

    pub fn to_bytes(&self) -> [u8; FUSE_FSYNC_IN_SIZE] {
        let mut out = [0u8; FUSE_FSYNC_IN_SIZE];
        out[0..8].copy_from_slice(&self.fh.to_le_bytes());
        out[8..12].copy_from_slice(&self.fsync_flags.to_le_bytes());
        out[12..16].copy_from_slice(&self.padding.to_le_bytes());
        out
    }
}

/// 8-byte access request (`struct fuse_access_in`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseAccessIn {
    pub mask: u32,
    pub padding: u32,
}

impl FuseAccessIn {
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < FUSE_ACCESS_IN_SIZE {
            return None;
        }
        Some(Self {
            mask: read_u32_le(buf, 0)?,
            padding: read_u32_le(buf, 4)?,
        })
    }

    pub fn to_bytes(&self) -> [u8; FUSE_ACCESS_IN_SIZE] {
        let mut out = [0u8; FUSE_ACCESS_IN_SIZE];
        out[0..4].copy_from_slice(&self.mask.to_le_bytes());
        out[4..8].copy_from_slice(&self.padding.to_le_bytes());
        out
    }
}

/// Extended attribute `setxattr` header (`struct fuse_setxattr_in`: 8B Linux, 16B macOS with `position`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseSetXattrIn {
    pub size: u32,
    pub flags: u32,
    /// Resource fork byte offset (macOS/Darwin ABI extension; 0 on Linux).
    pub position: u32,
    pub padding: u32,
}

impl FuseSetXattrIn {
    /// Parses a Linux (8B) or macOS (16B) `fuse_setxattr_in` header.
    pub fn parse_for(buf: &[u8], platform: FuseAbiPlatform) -> Option<(Self, usize)> {
        let need = platform.xattr_in_size();
        if buf.len() < need {
            return None;
        }
        let size = read_u32_le(buf, 0)?;
        let flags = read_u32_le(buf, 4)?;
        let (position, padding) = match platform {
            FuseAbiPlatform::Linux => (0, 0),
            FuseAbiPlatform::MacOs => (read_u32_le(buf, 8)?, read_u32_le(buf, 12)?),
        };
        Some((
            Self {
                size,
                flags,
                position,
                padding,
            },
            need,
        ))
    }

    pub fn to_bytes(&self) -> [u8; FUSE_XATTR_IN_SIZE] {
        let mut out = [0u8; FUSE_XATTR_IN_SIZE];
        out[0..4].copy_from_slice(&self.size.to_le_bytes());
        out[4..8].copy_from_slice(&self.flags.to_le_bytes());
        out
    }

    pub fn to_macos_bytes(&self) -> [u8; FUSE_XATTR_IN_MACOS_SIZE] {
        let mut out = [0u8; FUSE_XATTR_IN_MACOS_SIZE];
        out[0..4].copy_from_slice(&self.size.to_le_bytes());
        out[4..8].copy_from_slice(&self.flags.to_le_bytes());
        out[8..12].copy_from_slice(&self.position.to_le_bytes());
        out[12..16].copy_from_slice(&self.padding.to_le_bytes());
        out
    }
}

/// Extended attribute `getxattr` / `listxattr` header (`struct fuse_getxattr_in`: 8B Linux, 16B macOS with `position`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseGetXattrIn {
    pub size: u32,
    pub padding: u32,
    /// Resource fork byte offset (macOS/Darwin ABI extension; 0 on Linux).
    pub position: u32,
    pub padding2: u32,
}

impl FuseGetXattrIn {
    /// Parses a Linux (8B) or macOS (16B) `fuse_getxattr_in` header.
    pub fn parse_for(buf: &[u8], platform: FuseAbiPlatform) -> Option<(Self, usize)> {
        let need = platform.xattr_in_size();
        if buf.len() < need {
            return None;
        }
        let size = read_u32_le(buf, 0)?;
        let padding = read_u32_le(buf, 4)?;
        let (position, padding2) = match platform {
            FuseAbiPlatform::Linux => (0, 0),
            FuseAbiPlatform::MacOs => (read_u32_le(buf, 8)?, read_u32_le(buf, 12)?),
        };
        Some((
            Self {
                size,
                padding,
                position,
                padding2,
            },
            need,
        ))
    }

    pub fn to_bytes(&self) -> [u8; FUSE_XATTR_IN_SIZE] {
        let mut out = [0u8; FUSE_XATTR_IN_SIZE];
        out[0..4].copy_from_slice(&self.size.to_le_bytes());
        out[4..8].copy_from_slice(&self.padding.to_le_bytes());
        out
    }

    pub fn to_macos_bytes(&self) -> [u8; FUSE_XATTR_IN_MACOS_SIZE] {
        let mut out = [0u8; FUSE_XATTR_IN_MACOS_SIZE];
        out[0..4].copy_from_slice(&self.size.to_le_bytes());
        out[4..8].copy_from_slice(&self.padding.to_le_bytes());
        out[8..12].copy_from_slice(&self.position.to_le_bytes());
        out[12..16].copy_from_slice(&self.padding2.to_le_bytes());
        out
    }
}

/// 8-byte forget notification (`struct fuse_forget_in`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FuseForgetIn {
    pub nlookup: u64,
}

impl FuseForgetIn {
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < FUSE_FORGET_IN_SIZE {
            return None;
        }
        Some(Self {
            nlookup: read_u64_le(buf, 0)?,
        })
    }
}

/// On-wire `fuse_dirent` entry returned by `FUSE_READDIR`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuseDirEntry {
    /// FUSE node ID (`ino`).
    pub ino: u64,
    /// Monotonic 1-based directory offset cookie for the next entry.
    pub off: u64,
    /// Dirent file type (`FUSE_DT_DIR` or `FUSE_DT_REG`).
    pub file_type: u32,
    /// Entry filename (`.`, `..`, or child name).
    pub name: String,
}

impl FuseDirEntry {
    /// Exact 8-byte-aligned wire size of this `fuse_dirent` record.
    #[inline]
    pub fn wire_len(&self) -> usize {
        fuse_dirent_align(FUSE_DIRENT_HEADER_SIZE + self.name.len())
    }

    /// Appends this `fuse_dirent` record (24-byte header + UTF-8 name + zero padding to
    /// an 8-byte boundary) to `dst`.
    pub fn encode_into(&self, dst: &mut Vec<u8>) {
        let name_bytes = self.name.as_bytes();
        let unaligned = FUSE_DIRENT_HEADER_SIZE + name_bytes.len();
        let aligned = fuse_dirent_align(unaligned);
        dst.reserve(aligned);
        dst.extend_from_slice(&self.ino.to_le_bytes());
        dst.extend_from_slice(&self.off.to_le_bytes());
        dst.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
        dst.extend_from_slice(&self.file_type.to_le_bytes());
        dst.extend_from_slice(name_bytes);
        let pad = aligned - unaligned;
        if pad > 0 {
            dst.extend_from_slice(&[0u8; 8][..pad]);
        }
    }

    /// Parses a sequence of 8-byte-aligned `fuse_dirent` records from a `FUSE_READDIR` reply body.
    pub fn parse_stream(buf: &[u8]) -> Option<Vec<Self>> {
        let mut entries = Vec::new();
        let mut pos = 0usize;
        while pos < buf.len() {
            let rem = buf.get(pos..)?;
            if rem.len() < FUSE_DIRENT_HEADER_SIZE {
                return None;
            }
            let ino = read_u64_le(rem, 0)?;
            let off = read_u64_le(rem, 8)?;
            let namelen = read_u32_le(rem, 16)? as usize;
            let file_type = read_u32_le(rem, 20)?;
            let name_end = FUSE_DIRENT_HEADER_SIZE.checked_add(namelen)?;
            if name_end > rem.len() {
                return None;
            }
            let name = std::str::from_utf8(&rem[FUSE_DIRENT_HEADER_SIZE..name_end])
                .ok()?
                .to_string();
            let rec_len = name_end.checked_add(7).map(|x| x & !7)?;
            if rec_len > rem.len() {
                return None;
            }
            entries.push(Self {
                ino,
                off,
                file_type,
                name,
            });
            pos = pos.checked_add(rec_len)?;
        }
        Some(entries)
    }
}

// ---------------------------------------------------------------------------
// Mount options & open handle table
// ---------------------------------------------------------------------------

/// Configuration for mounting or serving an OIFS filesystem over FUSE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuseMountOptions {
    /// Enforce read-only FUSE access (`-libc::EROFS` on all mutations).
    pub read_only: bool,
    /// Compression mode applied to FUSE writes.
    pub compression_mode: CompressionMode,
    /// Owner UID reported in `FuseAttr` (defaults to current effective UID `geteuid()`).
    pub uid: u32,
    /// Owner GID reported in `FuseAttr` (defaults to current effective GID `getegid()`).
    pub gid: u32,
    /// Attribute cache TTL in seconds (`attr_valid`).
    pub attr_ttl_secs: u64,
    /// Entry cache TTL in seconds (`entry_valid`).
    pub entry_ttl_secs: u64,
    /// Pass `allow_other` mount option when mounting via kernel FUSE.
    pub allow_other: bool,
    /// Wire ABI platform layout (`Linux` 88-byte `FuseAttr` default, or `MacOs` 104-byte `FuseAttr`).
    pub abi_platform: FuseAbiPlatform,
    /// Fast-reject macOS Finder metadata files (`.DS_Store`, `._*`, `.Spotlight-V100`,
    /// `.Trashes`, `.fseventsd`, `.TemporaryItems`) with `-libc::ENOENT`. Defaults to `true`.
    pub reject_macos_metadata: bool,
    /// Normalize incoming UTF-8 NFD filenames (macOS APFS/Finder) to NFC. Defaults to `true`.
    pub normalize_nfc: bool,
    /// Optional volume display name on macOS (`volname=<name>`). Ignored on Linux.
    pub volname: Option<String>,
    /// Requested maximum single `FUSE_WRITE` payload size in bytes (default: 1 MiB).
    ///
    /// Automatically clamped to `[4 KiB, 4 MiB]` and dynamically negotiated with the
    /// host kernel during `FUSE_INIT` (e.g. capped to 128 KiB on Linux < 4.20 without
    /// `FUSE_MAX_PAGES`, or to `max_pages_limit * page_size` on Linux 4.20+/6.13+).
    /// On macOS, also emitted as `-o iosize=<bytes>`.
    pub max_write: u32,
}

impl Default for FuseMountOptions {
    fn default() -> Self {
        // SAFETY: `geteuid`/`getegid` are always safe to call and never fail.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        Self {
            read_only: false,
            compression_mode: CompressionMode::Auto,
            uid,
            gid,
            attr_ttl_secs: 1,
            entry_ttl_secs: 1,
            allow_other: false,
            abi_platform: FuseAbiPlatform::Linux,
            reject_macos_metadata: true,
            normalize_nfc: true,
            volname: None,
            max_write: FUSE_DEFAULT_MAX_WRITE,
        }
    }
}

impl FuseMountOptions {
    /// Returns default mount options configured with the host OS native FUSE ABI
    /// ([`FuseAbiPlatform::host`]).
    pub fn for_host_kernel() -> Self {
        Self {
            abi_platform: FuseAbiPlatform::host(),
            ..Self::default()
        }
    }

    /// Returns `self.max_write` clamped to `[FUSE_MIN_MAX_WRITE, FUSE_MAX_SUPPORTED_WRITE]`
    /// and aligned down to a 4 KiB block boundary.
    pub fn effective_max_write(&self) -> u32 {
        let clamped = self
            .max_write
            .clamp(FUSE_MIN_MAX_WRITE, FUSE_MAX_SUPPORTED_WRITE);
        (clamped / BLOCK_SIZE as u32) * BLOCK_SIZE as u32
    }

    /// Builds OS-filtered FUSE mount option strings (`-o ...`) for `target_os` (`"linux"` or `"macos"`).
    ///
    /// - Always includes `fsname=oifs` and `default_permissions`, plus `ro` when `read_only`
    ///   and `allow_other` when requested.
    /// - On `"macos"`, adds `noappledouble`, `noapplexattr`, `local`, `iosize=<max_write>`,
    ///   and `volname=<name>`.
    /// - On `"linux"`, adds `subtype=oifs` and strips macOS-only options (`noappledouble`,
    ///   `noapplexattr`, `volname`, `local`, `iosize`) so `fusermount3`/`fusermount` never
    ///   fail with `unknown option`.
    pub fn build_mount_options_for_os(
        &self,
        target_os: &str,
        effective_read_only: bool,
    ) -> Vec<String> {
        let mut opts = vec!["fsname=oifs".to_string(), "default_permissions".to_string()];
        if effective_read_only {
            opts.push("ro".to_string());
        }
        if self.allow_other {
            opts.push("allow_other".to_string());
        }
        match target_os {
            "macos" | "darwin" => {
                opts.push("noappledouble".to_string());
                opts.push("noapplexattr".to_string());
                opts.push("local".to_string());
                opts.push(format!("iosize={}", self.effective_max_write()));
                let name = self
                    .volname
                    .as_deref()
                    .filter(|s| !s.is_empty() && !s.contains(','))
                    .unwrap_or("oifs");
                opts.push(format!("volname={name}"));
            }
            "linux" => {
                opts.push("subtype=oifs".to_string());
            }
            _ => {}
        }
        opts
    }
}

/// Detects the Linux kernel FUSE `max_pages_limit` parameter dynamically.
///
/// - On Linux 6.13+, reads `/sys/module/fuse/parameters/max_pages_limit` (which defaults
///   to 256 and can be raised by administrators up to 1024).
/// - On Linux 4.20–6.12 (where the sysfs parameter does not exist) or non-Linux hosts,
///   returns the standard kernel `FUSE_MAX_MAX_PAGES` value (`256`).
pub fn detect_linux_fuse_max_pages_limit() -> u32 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(raw) = std::fs::read_to_string("/sys/module/fuse/parameters/max_pages_limit")
            && let Ok(parsed) = raw.trim().parse::<u32>()
        {
            return parsed.clamp(32, 1024);
        }
    }
    256
}

/// Returns the host OS memory page size in bytes (falling back to 4096 if unavailable).
pub fn host_page_size() -> u32 {
    // SAFETY: `sysconf(_SC_PAGESIZE)` has no preconditions and is thread-safe.
    let ps = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if ps > 0 && ps <= (1024 * 1024) {
        ps as u32
    } else {
        4096
    }
}

/// Dynamically negotiates `(max_write, max_pages)` for a `FUSE_INIT` handshake based on
/// the requested size, kernel-advertised `init_in.flags`, target ABI platform, host page
/// size, and Linux kernel `max_pages_limit`.
pub fn negotiate_max_write(
    requested: u32,
    init_flags: u32,
    platform: FuseAbiPlatform,
    page_size: u32,
    linux_max_pages_limit: u32,
) -> (u32, u16) {
    let page_size = page_size.max(4096);
    let requested = (requested.clamp(FUSE_MIN_MAX_WRITE, FUSE_MAX_SUPPORTED_WRITE)
        / BLOCK_SIZE as u32)
        * BLOCK_SIZE as u32;

    match platform {
        FuseAbiPlatform::Linux => {
            if (init_flags & FUSE_MAX_PAGES) == 0 {
                // Linux < 4.20 (no FUSE_MAX_PAGES): kernel hard-caps FUSE requests at 32 pages (128 KiB on 4K pages).
                let legacy_cap = 32u32.saturating_mul(page_size).max(FUSE_LEGACY_MAX_WRITE);
                let max_write = requested.min(legacy_cap);
                (max_write, 0)
            } else {
                // Linux 4.20+ (with FUSE_MAX_PAGES): cap by `max_pages_limit * page_size`.
                let limit_pages = linux_max_pages_limit.clamp(32, 1024);
                let kernel_cap = limit_pages.saturating_mul(page_size);
                let max_write = requested.min(kernel_cap);
                let max_pages = max_write.div_ceil(page_size).clamp(1, limit_pages) as u16;
                (max_write, max_pages)
            }
        }
        FuseAbiPlatform::MacOs => {
            if (init_flags & FUSE_BIG_WRITES) == 0 {
                (FUSE_MIN_MAX_WRITE, 0)
            } else {
                let max_pages = if (init_flags & FUSE_MAX_PAGES) != 0 {
                    max_write_to_pages(requested, page_size)
                } else {
                    0
                };
                (requested, max_pages)
            }
        }
    }
}

#[inline]
fn max_write_to_pages(max_write: u32, page_size: u32) -> u16 {
    max_write
        .div_ceil(page_size.max(4096))
        .clamp(1, u16::MAX as u32) as u16
}

/// Active open file or directory handle inside [`FuseHandleTable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuseFileHandle {
    /// OIFS inode ID backing this open handle.
    pub inode_id: u64,
    /// POSIX `open(2)` flags recorded when the handle was opened.
    pub flags: u32,
    /// `true` for directory handles (`FUSE_OPENDIR`), `false` for file handles.
    pub is_dir: bool,
}

/// Thread-safe handle table mapping FUSE `fh: u64` values to [`FuseFileHandle`] entries.
///
/// All open FUSE handles live in memory and share the single underlying `.img` OS file
/// descriptor owned by [`DiskManager`].
#[derive(Debug)]
pub struct FuseHandleTable {
    next_fh: AtomicU64,
    handles: RwLock<FxHashMap<u64, FuseFileHandle>>,
}

impl Default for FuseHandleTable {
    fn default() -> Self {
        Self::new()
    }
}

impl FuseHandleTable {
    /// Creates an empty handle table starting handle allocation at `fh = 1`.
    pub fn new() -> Self {
        Self {
            next_fh: AtomicU64::new(1),
            handles: RwLock::new(FxHashMap::default()),
        }
    }

    /// Inserts a new handle and returns its unique non-zero `fh` ID.
    pub fn insert(&self, handle: FuseFileHandle) -> u64 {
        let mut fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        if fh == 0 {
            fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        }
        let mut guard = crate::write_unpoisoned(&self.handles);
        while fh == 0 || guard.contains_key(&fh) {
            fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        }
        guard.insert(fh, handle);
        fh
    }

    /// Looks up an active handle by `fh`.
    pub fn get(&self, fh: u64) -> Option<FuseFileHandle> {
        crate::read_unpoisoned(&self.handles).get(&fh).copied()
    }

    /// Removes and returns an active handle by `fh`.
    pub fn remove(&self, fh: u64) -> Option<FuseFileHandle> {
        crate::write_unpoisoned(&self.handles).remove(&fh)
    }

    /// Returns the number of currently open handles.
    pub fn len(&self) -> usize {
        crate::read_unpoisoned(&self.handles).len()
    }

    /// Returns `true` if no handles are currently open.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drops all open handles (used on `FUSE_DESTROY`).
    pub fn clear(&self) {
        crate::write_unpoisoned(&self.handles).clear();
    }
}

// ---------------------------------------------------------------------------
// Cross-platform SCM_RIGHTS file descriptor passing & unmount helpers
// ---------------------------------------------------------------------------

/// Sends a file descriptor over a Unix domain socket using `SCM_RIGHTS` ancillary data.
pub fn send_fd_scm_rights(sock_fd: RawFd, fd: RawFd) -> std::io::Result<()> {
    let mut data_byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: data_byte.as_mut_ptr().cast(),
        iov_len: data_byte.len(),
    };
    let fd_size = std::mem::size_of::<RawFd>() as u32;
    // SAFETY: `CMSG_SPACE` is a pure size calculation macro wrapper.
    let cmsg_space = unsafe { libc::CMSG_SPACE(fd_size) } as usize;
    let mut cmsg_buf = vec![0u8; cmsg_space];

    // SAFETY: zeroed `msghdr` initialized with valid stack `iov` and heap `cmsg_buf`.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr().cast();
        msg.msg_controllen = cmsg_buf.len() as _;

        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(std::io::Error::other("CMSG_FIRSTHDR returned null"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(fd_size) as _;
        std::ptr::copy_nonoverlapping(
            (&fd as *const RawFd).cast::<u8>(),
            libc::CMSG_DATA(cmsg),
            std::mem::size_of::<RawFd>(),
        );

        loop {
            let sent = libc::sendmsg(sock_fd, &msg, 0);
            if sent >= 0 {
                return Ok(());
            }
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
    }
}

/// Receives a file descriptor sent over a Unix domain socket via `SCM_RIGHTS`
/// (used by Linux `fusermount3`/`fusermount` `_FUSE_COMMFD` handshakes and IPC tests).
pub fn recv_fd_scm_rights(sock_fd: RawFd) -> std::io::Result<OwnedFd> {
    let mut data_byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: data_byte.as_mut_ptr().cast(),
        iov_len: data_byte.len(),
    };
    let fd_size = std::mem::size_of::<RawFd>() as u32;
    // SAFETY: `CMSG_SPACE` is a pure size calculation.
    let cmsg_space = unsafe { libc::CMSG_SPACE(fd_size) } as usize;
    let mut cmsg_buf = vec![0u8; cmsg_space];

    // SAFETY: zeroed `msghdr` initialized with valid `iov` and `cmsg_buf` pointers.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr().cast();
        msg.msg_controllen = cmsg_buf.len() as _;

        loop {
            msg.msg_controllen = cmsg_buf.len() as _;
            let n = libc::recvmsg(sock_fd, &mut msg, 0);
            if n >= 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }

        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let mut received_fd: RawFd = -1;
                std::ptr::copy_nonoverlapping(
                    libc::CMSG_DATA(cmsg),
                    (&mut received_fd as *mut RawFd).cast::<u8>(),
                    std::mem::size_of::<RawFd>(),
                );
                if received_fd < 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "invalid file descriptor received via SCM_RIGHTS",
                    ));
                }
                return Ok(OwnedFd::from_raw_fd(received_fd));
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }

        Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "no SCM_RIGHTS file descriptor received on socket",
        ))
    }
}

/// Unmounts a FUSE filesystem at `mountpoint` using host OS tooling:
/// - **Linux**: tries `fusermount3 -u`, then `fusermount -u`, then `umount`.
/// - **macOS**: tries `umount`, then `diskutil unmount`.
pub fn unmount<P: AsRef<Path>>(mountpoint: P) -> std::io::Result<()> {
    let mp = mountpoint.as_ref();
    #[cfg(target_os = "linux")]
    let attempts: &[(&str, &[&str])] = &[
        ("fusermount3", &["-u", "--"]),
        ("fusermount", &["-u", "--"]),
        ("umount", &[]),
    ];
    #[cfg(target_os = "macos")]
    let attempts: &[(&str, &[&str])] = &[("umount", &[]), ("diskutil", &["unmount"])];
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let attempts: &[(&str, &[&str])] = &[("umount", &[])];

    let mut last_err = None;
    for &(prog, args) in attempts {
        match std::process::Command::new(prog).args(args).arg(mp).status() {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => {
                last_err = Some(std::io::Error::other(format!(
                    "{prog} failed to unmount {} (exit {status})",
                    mp.display()
                )));
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("failed to unmount FUSE mountpoint")))
}

// ---------------------------------------------------------------------------
// Error & POSIX flag helpers
// ---------------------------------------------------------------------------

/// Returns `true` if the POSIX open flags request write access or mutation.
#[inline]
pub fn open_flags_request_write(flags: u32) -> bool {
    let raw = flags as libc::c_int;
    let acc = raw & libc::O_ACCMODE;
    let write_mode = acc == libc::O_WRONLY || acc == libc::O_RDWR;
    let mutating_flags = (raw & (libc::O_CREAT | libc::O_TRUNC | libc::O_APPEND)) != 0;
    write_mode || mutating_flags
}

/// Computes the logical plaintext file size of an OIFS [`Inode`].
///
/// For uncompressed, non-seekable encrypted files, OIFS stores `plaintext_len + 16`
/// (including the 16-byte XChaCha20-Poly1305 AEAD tag) in `inode.size`.
#[inline]
fn logical_file_size(inode: &Inode) -> u64 {
    if inode.mode == FileType::File
        && inode.encrypted
        && inode.compressed_size == 0
        && (inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K) == 0
        && inode.size >= 16
    {
        inode.size - 16
    } else {
        inode.size
    }
}

/// Maps a [`DiskManagerError`] to a negative POSIX `errno` suitable for FUSE replies.
pub fn disk_error_to_errno(err: &DiskManagerError) -> i32 {
    match err {
        DiskManagerError::ReadOnly => -libc::EROFS,
        DiskManagerError::Allocator(AllocatorError::NoSpace) => -libc::ENOSPC,
        DiskManagerError::Allocator(AllocatorError::Io(e)) | DiskManagerError::Io(e) => {
            use std::io::ErrorKind;
            match e.kind() {
                ErrorKind::NotFound => -libc::ENOENT,
                ErrorKind::AlreadyExists => -libc::EEXIST,
                ErrorKind::PermissionDenied => -libc::EACCES,
                ErrorKind::InvalidInput => {
                    let msg = e.to_string();
                    if msg.contains("too large") || msg.contains("Entry too large") {
                        -libc::ENAMETOOLONG
                    } else {
                        -libc::EINVAL
                    }
                }
                ErrorKind::OutOfMemory => -libc::ENOMEM,
                _ => {
                    let msg = e.to_string();
                    if msg.contains("Directory is not empty") || msg.contains("not empty") {
                        -libc::ENOTEMPTY
                    } else if msg.contains("Not a directory") {
                        -libc::ENOTDIR
                    } else if msg.contains("Cannot read non-file")
                        || msg.contains("Cannot write to non-file")
                        || msg.contains("Is a directory")
                        || msg.contains("Cannot truncate a directory")
                    {
                        -libc::EISDIR
                    } else if msg.contains("out of bounds")
                        || msg.contains("Inode decode failed")
                        || msg.contains("File not found")
                    {
                        -libc::ENOENT
                    } else if msg.contains("Entry too large") {
                        -libc::ENAMETOOLONG
                    } else if let Some(code) = e.raw_os_error() {
                        -code
                    } else {
                        -libc::EIO
                    }
                }
            }
        }
        DiskManagerError::PasswordRequired | DiskManagerError::DecryptionFailed => -libc::EACCES,
        _ => -libc::EIO,
    }
}

fn parse_fuse_name(bytes: &[u8]) -> Result<&str, i32> {
    let raw = match bytes.iter().position(|&b| b == 0) {
        Some(pos) => &bytes[..pos],
        None => bytes,
    };
    if raw.is_empty() {
        return Err(-libc::EINVAL);
    }
    if raw.len() > DirectoryEntry::MAX_FILENAME_LEN {
        return Err(-libc::ENAMETOOLONG);
    }
    std::str::from_utf8(raw).map_err(|_| -libc::EINVAL)
}

fn parse_fuse_two_names(bytes: &[u8]) -> Result<(&str, &str), i32> {
    let first_nul = bytes.iter().position(|&b| b == 0).ok_or(-libc::EINVAL)?;
    let old_name = parse_fuse_name(&bytes[..first_nul])?;
    let rest = bytes.get(first_nul + 1..).ok_or(-libc::EINVAL)?;
    let new_name = parse_fuse_name(rest)?;
    Ok((old_name, new_name))
}

fn count_bitmap_bits_used(bitmap: &[u8], total_bits: usize) -> u64 {
    let bits_to_scan = total_bits.min(bitmap.len() * 8);
    let full_words = bits_to_scan / 64;
    let rem_bits = bits_to_scan % 64;
    let mut used = 0u64;
    for w in 0..full_words {
        let off = w * 8;
        let word = u64::from_le_bytes([
            bitmap[off],
            bitmap[off + 1],
            bitmap[off + 2],
            bitmap[off + 3],
            bitmap[off + 4],
            bitmap[off + 5],
            bitmap[off + 6],
            bitmap[off + 7],
        ]);
        used += word.count_ones() as u64;
    }
    if rem_bits > 0 {
        let bit_base = full_words * 64;
        for b in 0..rem_bits {
            let bit_idx = bit_base + b;
            if (bitmap[bit_idx / 8] & (1u8 << (bit_idx % 8))) != 0 {
                used += 1;
            }
        }
    }
    used
}

fn count_allocated_inodes(dm: &DiskManager) -> u64 {
    let sb = dm.superblock();
    let total_inodes = sb.inode_count as usize;
    let bitmap_blocks = sb.inode_bitmap_blocks();

    // Under `StorageMode::FileIo`, bitmap writes go directly through `BitmapSliceMut`
    // to disk without invalidating `MetadataBlockCache` (used by `get_block_copy`),
    // so read the write-through inode bitmap directly via positional `pread`.
    if dm.storage_mode() == crate::storage::StorageMode::FileIo {
        use std::os::unix::fs::FileExt;
        let byte_len = (bitmap_blocks as usize).saturating_mul(BLOCK_SIZE);
        let byte_off = sb.inode_bitmap_block.saturating_mul(BLOCK_SIZE as u64);
        if let Ok(file) = std::fs::File::open(dm.backing_path()) {
            let mut buf = vec![0u8; byte_len];
            if file.read_exact_at(&mut buf, byte_off).is_ok() {
                return count_bitmap_bits_used(&buf, total_inodes);
            }
        }
    }

    let mut used = 0u64;
    let mut counted_bits = 0usize;
    for blk_off in 0..bitmap_blocks {
        if counted_bits >= total_inodes {
            break;
        }
        let Some(block) = dm.get_block_copy(sb.inode_bitmap_block + blk_off) else {
            break;
        };
        let remaining = total_inodes - counted_bits;
        let bits_in_block = remaining.min(block.len() * 8);
        used += count_bitmap_bits_used(&block, bits_in_block);
        counted_bits += bits_in_block;
    }
    used
}

// ---------------------------------------------------------------------------
// Stateful FUSE session (decoupled from DiskManager ownership for FFI reuse)
// ---------------------------------------------------------------------------

/// Stateful FUSE v7.28 protocol session that can be driven against any `&DiskManager`.
///
/// This type holds the mount options, open-handle table (`fh -> FuseFileHandle`), parent
/// directory tracking for accurate `..` entries, and `FUSE_INIT` negotiation state.
/// [`OifsFuseFs`] embeds an `OifsFuseState` together with an `Arc<DiskManager>`, while
/// FFI wrappers that already own a `DiskManager` can embed `OifsFuseState` directly.
pub struct OifsFuseState {
    options: FuseMountOptions,
    handles: FuseHandleTable,
    parent_map: RwLock<FxHashMap<u64, u64>>,
    append_lock: Mutex<()>,
    initialized: AtomicBool,
    destroyed: AtomicBool,
    max_write: AtomicU32,
}

impl OifsFuseState {
    /// Creates a fresh FUSE protocol state with the given [`FuseMountOptions`].
    pub fn new(options: FuseMountOptions) -> Self {
        let mut parents = FxHashMap::default();
        parents.insert(0, 0);
        Self {
            options,
            handles: FuseHandleTable::new(),
            parent_map: RwLock::new(parents),
            append_lock: Mutex::new(()),
            initialized: AtomicBool::new(false),
            destroyed: AtomicBool::new(false),
            max_write: AtomicU32::new(FUSE_DEFAULT_MAX_WRITE),
        }
    }

    /// Returns the configured [`FuseMountOptions`].
    #[inline]
    pub fn options(&self) -> &FuseMountOptions {
        &self.options
    }

    /// Returns `true` if mutations are forbidden for this FUSE mount.
    #[inline]
    pub fn is_read_only(&self, dm: &DiskManager) -> bool {
        dm.is_read_only() || self.options.read_only
    }

    /// Returns the number of currently open FUSE file and directory handles.
    #[inline]
    pub fn open_handle_count(&self) -> usize {
        self.handles.len()
    }

    /// Returns `true` if `FUSE_INIT` has completed.
    #[inline]
    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    /// Returns `true` if `FUSE_DESTROY` has been processed.
    #[inline]
    pub fn is_destroyed(&self) -> bool {
        self.destroyed.load(Ordering::Acquire)
    }

    #[inline]
    fn normalize_name<'a>(&self, name: &'a str) -> Cow<'a, str> {
        if self.options.normalize_nfc {
            normalize_filename_nfc(name)
        } else {
            Cow::Borrowed(name)
        }
    }

    #[inline]
    fn reject_if_macos_metadata(&self, name: &str) -> Result<(), i32> {
        if self.options.reject_macos_metadata && is_macos_metadata_filename(name) {
            return Err(-libc::ENOENT);
        }
        Ok(())
    }

    #[inline]
    fn record_parent(&self, child_inode: u64, parent_inode: u64) {
        crate::write_unpoisoned(&self.parent_map).insert(child_inode, parent_inode);
    }

    #[inline]
    fn forget_parent(&self, child_inode: u64) {
        crate::write_unpoisoned(&self.parent_map).remove(&child_inode);
    }

    #[inline]
    fn parent_nodeid_of(&self, inode_id: u64) -> u64 {
        let parent_inode = crate::read_unpoisoned(&self.parent_map)
            .get(&inode_id)
            .copied()
            .unwrap_or(0);
        inode_to_nodeid(parent_inode).unwrap_or(FUSE_ROOT_ID)
    }

    fn build_entry_out(
        &self,
        dm: &DiskManager,
        inode_id: u64,
        inode: &Inode,
    ) -> Result<FuseEntryOut, i32> {
        let nodeid = inode_to_nodeid(inode_id).ok_or(-libc::EOVERFLOW)?;
        let attr = FuseAttr::from_inode(nodeid, inode, &self.options, self.is_read_only(dm));
        Ok(FuseEntryOut {
            nodeid,
            generation: 0,
            entry_valid: self.options.entry_ttl_secs,
            attr_valid: self.options.attr_ttl_secs,
            entry_valid_nsec: 0,
            attr_valid_nsec: 0,
            attr,
        })
    }

    fn build_attr_out(
        &self,
        dm: &DiskManager,
        inode_id: u64,
        inode: &Inode,
    ) -> Result<FuseAttrOut, i32> {
        let nodeid = inode_to_nodeid(inode_id).ok_or(-libc::EOVERFLOW)?;
        let attr = FuseAttr::from_inode(nodeid, inode, &self.options, self.is_read_only(dm));
        Ok(FuseAttrOut {
            attr_valid: self.options.attr_ttl_secs,
            attr_valid_nsec: 0,
            dummy: 0,
            attr,
        })
    }

    fn resolve_file_inode(&self, dm: &DiskManager, nodeid: u64) -> Result<(u64, Inode), i32> {
        let inode_id = nodeid_to_inode(nodeid).ok_or(-libc::ENOENT)?;
        let inode = dm
            .read_inode(inode_id)
            .map_err(|e| disk_error_to_errno(&e))?;
        Ok((inode_id, inode))
    }

    pub fn lookup(&self, dm: &DiskManager, parent: u64, name: &str) -> Result<FuseEntryOut, i32> {
        let name = self.normalize_name(name);
        self.reject_if_macos_metadata(name.as_ref())?;
        let parent_inode = nodeid_to_inode(parent).ok_or(-libc::ENOENT)?;
        let child_inode = dm
            .lookup(parent_inode, name.as_ref())
            .map_err(|e| disk_error_to_errno(&e))?;
        let inode = dm
            .read_inode(child_inode)
            .map_err(|e| disk_error_to_errno(&e))?;
        if inode.mode == FileType::Directory {
            self.record_parent(child_inode, parent_inode);
        }
        self.build_entry_out(dm, child_inode, &inode)
    }

    pub fn getattr(&self, dm: &DiskManager, nodeid: u64) -> Result<FuseAttrOut, i32> {
        let (inode_id, inode) = self.resolve_file_inode(dm, nodeid)?;
        self.build_attr_out(dm, inode_id, &inode)
    }

    pub fn setattr_size(
        &self,
        dm: &DiskManager,
        nodeid: u64,
        new_size: u64,
    ) -> Result<FuseAttrOut, i32> {
        if self.is_read_only(dm) {
            return Err(-libc::EROFS);
        }
        let (inode_id, inode) = self.resolve_file_inode(dm, nodeid)?;
        if inode.mode == FileType::Directory {
            return Err(-libc::EISDIR);
        }
        dm.truncate(inode_id, new_size)
            .map_err(|e| disk_error_to_errno(&e))?;
        let updated = dm
            .read_inode(inode_id)
            .map_err(|e| disk_error_to_errno(&e))?;
        self.build_attr_out(dm, inode_id, &updated)
    }

    pub fn mkdir(
        &self,
        dm: &DiskManager,
        parent: u64,
        name: &str,
        _mode: u32,
    ) -> Result<FuseEntryOut, i32> {
        if self.is_read_only(dm) {
            return Err(-libc::EROFS);
        }
        let name = self.normalize_name(name);
        self.reject_if_macos_metadata(name.as_ref())?;
        let parent_inode = nodeid_to_inode(parent).ok_or(-libc::ENOENT)?;
        let child_inode = dm
            .create_directory(parent_inode, name.as_ref())
            .map_err(|e| disk_error_to_errno(&e))?;
        self.record_parent(child_inode, parent_inode);
        let inode = dm
            .read_inode(child_inode)
            .map_err(|e| disk_error_to_errno(&e))?;
        self.build_entry_out(dm, child_inode, &inode)
    }

    pub fn unlink(&self, dm: &DiskManager, parent: u64, name: &str) -> Result<(), i32> {
        if self.is_read_only(dm) {
            return Err(-libc::EROFS);
        }
        let name = self.normalize_name(name);
        self.reject_if_macos_metadata(name.as_ref())?;
        let parent_inode = nodeid_to_inode(parent).ok_or(-libc::ENOENT)?;
        let target_inode_id = dm
            .lookup(parent_inode, name.as_ref())
            .map_err(|e| disk_error_to_errno(&e))?;
        let target_inode = dm
            .read_inode(target_inode_id)
            .map_err(|e| disk_error_to_errno(&e))?;
        if target_inode.mode == FileType::Directory {
            return Err(-libc::EISDIR);
        }
        dm.delete_file(parent_inode, name.as_ref())
            .map_err(|e| disk_error_to_errno(&e))
    }

    pub fn rmdir(&self, dm: &DiskManager, parent: u64, name: &str) -> Result<(), i32> {
        if self.is_read_only(dm) {
            return Err(-libc::EROFS);
        }
        let name = self.normalize_name(name);
        self.reject_if_macos_metadata(name.as_ref())?;
        let parent_inode = nodeid_to_inode(parent).ok_or(-libc::ENOENT)?;
        let target_inode_id = dm
            .lookup(parent_inode, name.as_ref())
            .map_err(|e| disk_error_to_errno(&e))?;
        let target_inode = dm
            .read_inode(target_inode_id)
            .map_err(|e| disk_error_to_errno(&e))?;
        if target_inode.mode != FileType::Directory {
            return Err(-libc::ENOTDIR);
        }
        dm.delete_file(parent_inode, name.as_ref())
            .map_err(|e| disk_error_to_errno(&e))?;
        self.forget_parent(target_inode_id);
        Ok(())
    }

    pub fn rename(
        &self,
        dm: &DiskManager,
        parent: u64,
        name: &str,
        newparent: u64,
        newname: &str,
        flags: u32,
    ) -> Result<(), i32> {
        if self.is_read_only(dm) {
            return Err(-libc::EROFS);
        }
        if flags != 0 && flags != FUSE_RENAME_NOREPLACE {
            return Err(-libc::EINVAL);
        }
        let name = self.normalize_name(name);
        let newname = self.normalize_name(newname);
        self.reject_if_macos_metadata(name.as_ref())?;
        self.reject_if_macos_metadata(newname.as_ref())?;
        let old_parent_inode = nodeid_to_inode(parent).ok_or(-libc::ENOENT)?;
        let new_parent_inode = nodeid_to_inode(newparent).ok_or(-libc::ENOENT)?;
        if flags == FUSE_RENAME_NOREPLACE && dm.lookup(new_parent_inode, newname.as_ref()).is_ok() {
            return Err(-libc::EEXIST);
        }
        let moved_inode_id = dm
            .lookup(old_parent_inode, name.as_ref())
            .map_err(|e| disk_error_to_errno(&e))?;
        let overwritten_inode_id = dm.lookup(new_parent_inode, newname.as_ref()).ok();
        dm.rename(
            old_parent_inode,
            name.as_ref(),
            new_parent_inode,
            newname.as_ref(),
        )
        .map_err(|e| disk_error_to_errno(&e))?;
        if let Some(dst_id) = overwritten_inode_id
            && dst_id != moved_inode_id
        {
            self.forget_parent(dst_id);
        }
        if let Ok(inode) = dm.read_inode(moved_inode_id)
            && inode.mode == FileType::Directory
        {
            self.record_parent(moved_inode_id, new_parent_inode);
        }
        Ok(())
    }

    pub fn open(&self, dm: &DiskManager, nodeid: u64, flags: u32) -> Result<FuseOpenOut, i32> {
        if self.is_read_only(dm) && open_flags_request_write(flags) {
            return Err(-libc::EROFS);
        }
        let (inode_id, inode) = self.resolve_file_inode(dm, nodeid)?;
        if inode.mode == FileType::Directory {
            return Err(-libc::EISDIR);
        }
        if ((flags as libc::c_int) & libc::O_TRUNC) != 0 {
            dm.truncate(inode_id, 0)
                .map_err(|e| disk_error_to_errno(&e))?;
        }
        let fh = self.handles.insert(FuseFileHandle {
            inode_id,
            flags,
            is_dir: false,
        });
        Ok(FuseOpenOut {
            fh,
            open_flags: 0,
            padding: 0,
        })
    }

    pub fn create(
        &self,
        dm: &DiskManager,
        parent: u64,
        name: &str,
        flags: u32,
        _mode: u32,
    ) -> Result<(FuseEntryOut, FuseOpenOut), i32> {
        if self.is_read_only(dm) {
            return Err(-libc::EROFS);
        }
        let name = self.normalize_name(name);
        self.reject_if_macos_metadata(name.as_ref())?;
        let parent_inode = nodeid_to_inode(parent).ok_or(-libc::ENOENT)?;
        let excl = ((flags as libc::c_int) & libc::O_EXCL) != 0;
        let trunc = ((flags as libc::c_int) & libc::O_TRUNC) != 0;

        let inode_id = if excl {
            dm.create_file(parent_inode, name.as_ref())
                .map_err(|e| disk_error_to_errno(&e))?
        } else {
            dm.create_or_open_file(parent_inode, name.as_ref())
                .map_err(|e| disk_error_to_errno(&e))?
        };
        let mut inode = dm
            .read_inode(inode_id)
            .map_err(|e| disk_error_to_errno(&e))?;
        if inode.mode == FileType::Directory {
            return Err(-libc::EISDIR);
        }
        if trunc && inode.size > 0 {
            dm.truncate(inode_id, 0)
                .map_err(|e| disk_error_to_errno(&e))?;
            inode = dm
                .read_inode(inode_id)
                .map_err(|e| disk_error_to_errno(&e))?;
        }
        let fh = self.handles.insert(FuseFileHandle {
            inode_id,
            flags,
            is_dir: false,
        });
        let entry = self.build_entry_out(dm, inode_id, &inode)?;
        let open_out = FuseOpenOut {
            fh,
            open_flags: 0,
            padding: 0,
        };
        Ok((entry, open_out))
    }

    pub fn read(
        &self,
        dm: &DiskManager,
        nodeid: u64,
        fh: u64,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>, i32> {
        let inode_id = if fh != 0 {
            let handle = self.handles.get(fh).ok_or(-libc::EBADF)?;
            if handle.is_dir {
                return Err(-libc::EISDIR);
            }
            handle.inode_id
        } else {
            nodeid_to_inode(nodeid).ok_or(-libc::ENOENT)?
        };
        let inode = dm
            .read_inode(inode_id)
            .map_err(|e| disk_error_to_errno(&e))?;
        if inode.mode == FileType::Directory {
            return Err(-libc::EISDIR);
        }
        let logical_size = logical_file_size(&inode);
        if offset >= logical_size || size == 0 {
            return Ok(Vec::new());
        }
        let available = usize::try_from(logical_size - offset).unwrap_or(usize::MAX);
        let to_read = (size as usize).min(available).min(FUSE_MAX_FRAME_LEN);
        let mut buf = vec![0u8; to_read];
        let n = dm
            .read_at(inode_id, offset, &mut buf)
            .map_err(|e| disk_error_to_errno(&e))?;
        buf.truncate(n);
        Ok(buf)
    }

    pub fn write(
        &self,
        dm: &DiskManager,
        nodeid: u64,
        fh: u64,
        offset: u64,
        data: &[u8],
        flags: u32,
    ) -> Result<u32, i32> {
        if self.is_read_only(dm) {
            return Err(-libc::EROFS);
        }
        let (inode_id, handle_flags) = if fh != 0 {
            let handle = self.handles.get(fh).ok_or(-libc::EBADF)?;
            if handle.is_dir {
                return Err(-libc::EISDIR);
            }
            (handle.inode_id, handle.flags)
        } else {
            let id = nodeid_to_inode(nodeid).ok_or(-libc::ENOENT)?;
            (id, 0)
        };

        let is_append = (((handle_flags | flags) as libc::c_int) & libc::O_APPEND) != 0;
        let written_len = u32::try_from(data.len()).map_err(|_| -libc::EFBIG)?;

        if is_append {
            let _append_guard = crate::lock_unpoisoned(&self.append_lock);
            let inode = dm
                .read_inode(inode_id)
                .map_err(|e| disk_error_to_errno(&e))?;
            if inode.mode == FileType::Directory {
                return Err(-libc::EISDIR);
            }
            let append_off = logical_file_size(&inode);
            dm.write_data(inode_id, append_off, data, self.options.compression_mode)
                .map_err(|e| disk_error_to_errno(&e))?;
        } else {
            let inode = dm
                .read_inode(inode_id)
                .map_err(|e| disk_error_to_errno(&e))?;
            if inode.mode == FileType::Directory {
                return Err(-libc::EISDIR);
            }
            dm.write_data(inode_id, offset, data, self.options.compression_mode)
                .map_err(|e| disk_error_to_errno(&e))?;
        }
        Ok(written_len)
    }

    pub fn flush(&self, dm: &DiskManager, nodeid: u64, fh: u64) -> Result<(), i32> {
        if fh != 0 {
            let _ = self.handles.get(fh).ok_or(-libc::EBADF)?;
        } else {
            let _ = self.resolve_file_inode(dm, nodeid)?;
        }
        if !self.is_read_only(dm) {
            dm.flush_async().map_err(|e| disk_error_to_errno(&e))?;
        }
        Ok(())
    }

    pub fn fsync(
        &self,
        dm: &DiskManager,
        nodeid: u64,
        fh: u64,
        _datasync: bool,
    ) -> Result<(), i32> {
        if fh != 0 {
            let _ = self.handles.get(fh).ok_or(-libc::EBADF)?;
        } else {
            let _ = self.resolve_file_inode(dm, nodeid)?;
        }
        if !self.is_read_only(dm) {
            dm.flush().map_err(|e| disk_error_to_errno(&e))?;
        }
        Ok(())
    }

    pub fn release(&self, _nodeid: u64, fh: u64) -> Result<(), i32> {
        if fh != 0 {
            let _ = self.handles.remove(fh);
        }
        Ok(())
    }

    pub fn opendir(&self, dm: &DiskManager, nodeid: u64, flags: u32) -> Result<FuseOpenOut, i32> {
        let (inode_id, inode) = self.resolve_file_inode(dm, nodeid)?;
        if inode.mode != FileType::Directory {
            return Err(-libc::ENOTDIR);
        }
        let fh = self.handles.insert(FuseFileHandle {
            inode_id,
            flags,
            is_dir: true,
        });
        Ok(FuseOpenOut {
            fh,
            open_flags: 0,
            padding: 0,
        })
    }

    pub fn readdir(
        &self,
        dm: &DiskManager,
        nodeid: u64,
        fh: u64,
        offset: u64,
        max_bytes: u32,
    ) -> Result<Vec<FuseDirEntry>, i32> {
        let inode_id = if fh != 0 {
            let handle = self.handles.get(fh).ok_or(-libc::EBADF)?;
            if !handle.is_dir {
                return Err(-libc::ENOTDIR);
            }
            handle.inode_id
        } else {
            nodeid_to_inode(nodeid).ok_or(-libc::ENOENT)?
        };
        let inode = dm
            .read_inode(inode_id)
            .map_err(|e| disk_error_to_errno(&e))?;
        if inode.mode != FileType::Directory {
            return Err(-libc::ENOTDIR);
        }
        let dir_nodeid = inode_to_nodeid(inode_id).ok_or(-libc::EOVERFLOW)?;
        let parent_nodeid = self.parent_nodeid_of(inode_id);

        let mut dir_entries = dm.list_dir(inode_id).map_err(|e| disk_error_to_errno(&e))?;
        dir_entries.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.inode.cmp(&b.inode)));

        let mut all = Vec::with_capacity(dir_entries.len() + 2);
        all.push(FuseDirEntry {
            ino: dir_nodeid,
            off: 1,
            file_type: FUSE_DT_DIR,
            name: ".".to_string(),
        });
        all.push(FuseDirEntry {
            ino: parent_nodeid,
            off: 2,
            file_type: FUSE_DT_DIR,
            name: "..".to_string(),
        });
        for (idx, entry) in dir_entries.into_iter().enumerate() {
            let child_nodeid = inode_to_nodeid(entry.inode).ok_or(-libc::EOVERFLOW)?;
            let file_type = match dm.read_inode(entry.inode) {
                Ok(child_inode) if child_inode.mode == FileType::Directory => {
                    self.record_parent(entry.inode, inode_id);
                    FUSE_DT_DIR
                }
                _ => FUSE_DT_REG,
            };
            all.push(FuseDirEntry {
                ino: child_nodeid,
                off: (idx as u64) + 3,
                file_type,
                name: entry.name,
            });
        }

        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        if start >= all.len() || max_bytes == 0 {
            return Ok(Vec::new());
        }

        let budget = max_bytes as usize;
        let mut used = 0usize;
        let mut page = Vec::new();
        for entry in all.into_iter().skip(start) {
            let rec_len = entry.wire_len();
            if used.saturating_add(rec_len) > budget {
                break;
            }
            used += rec_len;
            page.push(entry);
        }
        Ok(page)
    }

    pub fn releasedir(&self, _nodeid: u64, fh: u64) -> Result<(), i32> {
        if fh != 0 {
            let _ = self.handles.remove(fh);
        }
        Ok(())
    }

    pub fn statfs(&self, dm: &DiskManager, nodeid: u64) -> Result<FuseKStatfs, i32> {
        let _ = self.resolve_file_inode(dm, nodeid)?;
        let sb = dm.superblock();
        let frag = dm
            .analyze_fragmentation()
            .map_err(|e| disk_error_to_errno(&e))?;
        let used_inodes = count_allocated_inodes(dm);
        let free_inodes = sb.inode_count.saturating_sub(used_inodes);
        let bfree = frag.free_blocks as u64;
        let bavail = if self.is_read_only(dm) { 0 } else { bfree };
        Ok(FuseKStatfs {
            blocks: frag.total_blocks as u64,
            bfree,
            bavail,
            files: sb.inode_count,
            ffree: free_inodes,
            bsize: sb.block_size,
            namelen: DirectoryEntry::MAX_FILENAME_LEN as u32,
            frsize: sb.block_size,
            padding: 0,
            spare: [0u32; 6],
        })
    }

    pub fn access(&self, dm: &DiskManager, nodeid: u64, mask: u32) -> Result<(), i32> {
        let _ = self.resolve_file_inode(dm, nodeid)?;
        if self.is_read_only(dm) && ((mask as libc::c_int) & libc::W_OK) != 0 {
            return Err(-libc::EROFS);
        }
        Ok(())
    }

    /// Extended attributes are unsupported on OIFS; returns `-libc::ENOTSUP` (never
    /// `-libc::ENOSYS` or `-libc::EINVAL`, which would cause macOS Finder copies to abort).
    pub fn setxattr(
        &self,
        dm: &DiskManager,
        nodeid: u64,
        _name: &str,
        _value: &[u8],
        _flags: u32,
    ) -> Result<(), i32> {
        if self.is_read_only(dm) {
            return Err(-libc::EROFS);
        }
        let _ = self.resolve_file_inode(dm, nodeid)?;
        Err(-libc::ENOTSUP)
    }

    /// Extended attributes are unsupported on OIFS; returns `-libc::ENOTSUP`.
    pub fn getxattr(
        &self,
        dm: &DiskManager,
        nodeid: u64,
        _name: &str,
        _size: u32,
    ) -> Result<Vec<u8>, i32> {
        let _ = self.resolve_file_inode(dm, nodeid)?;
        Err(-libc::ENOTSUP)
    }

    /// Extended attributes are unsupported on OIFS; returns `-libc::ENOTSUP`.
    pub fn listxattr(&self, dm: &DiskManager, nodeid: u64, _size: u32) -> Result<Vec<u8>, i32> {
        let _ = self.resolve_file_inode(dm, nodeid)?;
        Err(-libc::ENOTSUP)
    }

    /// Extended attributes are unsupported on OIFS; returns `-libc::ENOTSUP` (or `-libc::EROFS` when read-only).
    pub fn removexattr(&self, dm: &DiskManager, nodeid: u64, _name: &str) -> Result<(), i32> {
        if self.is_read_only(dm) {
            return Err(-libc::EROFS);
        }
        let _ = self.resolve_file_inode(dm, nodeid)?;
        Err(-libc::ENOTSUP)
    }

    fn handle_init(
        &self,
        init_in: FuseInitIn,
        platform: FuseAbiPlatform,
    ) -> Result<FuseInitOut, i32> {
        if init_in.major < FUSE_KERNEL_VERSION {
            return Err(-libc::EPROTO);
        }
        if init_in.major > FUSE_KERNEL_VERSION {
            return Ok(FuseInitOut {
                major: FUSE_KERNEL_VERSION,
                minor: FUSE_KERNEL_MINOR_VERSION,
                ..FuseInitOut::default()
            });
        }
        let minor = init_in.minor.min(FUSE_KERNEL_MINOR_VERSION);
        let max_readahead = init_in.max_readahead.min(FUSE_DEFAULT_MAX_READAHEAD);
        let supported_flags =
            FUSE_ASYNC_READ | FUSE_BIG_WRITES | FUSE_ATOMIC_O_TRUNC | FUSE_MAX_PAGES;
        let negotiated_flags = init_in.flags & supported_flags;
        let (max_write, max_pages) = negotiate_max_write(
            self.options.effective_max_write(),
            init_in.flags,
            platform,
            host_page_size(),
            detect_linux_fuse_max_pages_limit(),
        );
        self.max_write.store(max_write, Ordering::Release);
        self.initialized.store(true, Ordering::Release);
        self.destroyed.store(false, Ordering::Release);
        Ok(FuseInitOut {
            major: FUSE_KERNEL_VERSION,
            minor,
            max_readahead,
            flags: negotiated_flags,
            max_background: 16,
            congestion_threshold: 12,
            max_write,
            time_gran: 1_000_000_000,
            max_pages,
            map_alignment: 0,
            unused: [0u32; 8],
        })
    }

    /// Dispatches a raw FUSE Kernel ABI v7.28 binary request frame against `dm` using the
    /// configured [`FuseMountOptions::abi_platform`].
    pub fn dispatch_frame(&self, dm: &DiskManager, request: &[u8]) -> Vec<u8> {
        self.dispatch_frame_for_platform(dm, request, self.options.abi_platform)
    }

    /// Dispatches a raw FUSE Kernel ABI v7.28 binary request frame against `dm` using an
    /// explicit [`FuseAbiPlatform`] (`Linux` 88-byte `FuseAttr` or `MacOs` 104-byte `FuseAttr`).
    ///
    /// Returns the serialized `FuseOutHeader` + body, or an empty `Vec` for one-way
    /// notifications (`FUSE_FORGET`, `FUSE_BATCH_FORGET`). Never panics on any input slice.
    pub fn dispatch_frame_for_platform(
        &self,
        dm: &DiskManager,
        request: &[u8],
        platform: FuseAbiPlatform,
    ) -> Vec<u8> {
        let Some(hdr) = FuseInHeader::parse(request) else {
            let unique = if request.len() >= 16 {
                read_u64_le(request, 8).unwrap_or(0)
            } else {
                0
            };
            return FuseOutHeader::error_frame(unique, -libc::EINVAL);
        };
        let payload = &request[FUSE_IN_HEADER_SIZE..];
        let unique = hdr.unique;

        let Some(opcode) = FuseOpcode::from_u32(hdr.opcode) else {
            return FuseOutHeader::error_frame(unique, -libc::ENOSYS);
        };

        match opcode {
            FuseOpcode::Init => {
                let Some(init_in) = FuseInitIn::parse(payload) else {
                    return FuseOutHeader::error_frame(unique, -libc::EINVAL);
                };
                match self.handle_init(init_in, platform) {
                    Ok(out) => FuseOutHeader::ok_frame(unique, &out.to_bytes()),
                    Err(e) => FuseOutHeader::error_frame(unique, e),
                }
            }
            FuseOpcode::Destroy => {
                self.handles.clear();
                self.destroyed.store(true, Ordering::Release);
                if !self.is_read_only(dm) {
                    let _ = dm.flush();
                }
                FuseOutHeader::ok_frame(unique, &[])
            }
            FuseOpcode::Forget => {
                let _ = FuseForgetIn::parse(payload);
                Vec::new()
            }
            FuseOpcode::BatchForget => Vec::new(),
            FuseOpcode::Lookup => {
                let name = match parse_fuse_name(payload) {
                    Ok(n) => n,
                    Err(e) => return FuseOutHeader::error_frame(unique, e),
                };
                match self.lookup(dm, hdr.nodeid, name) {
                    Ok(entry) => FuseOutHeader::ok_frame(unique, &entry.to_bytes_for(platform)),
                    Err(e) => FuseOutHeader::error_frame(unique, e),
                }
            }
            FuseOpcode::Getattr => match self.getattr(dm, hdr.nodeid) {
                Ok(attr_out) => FuseOutHeader::ok_frame(unique, &attr_out.to_bytes_for(platform)),
                Err(e) => FuseOutHeader::error_frame(unique, e),
            },
            FuseOpcode::Setattr => {
                let Some(setattr_in) = FuseSetAttrIn::parse(payload) else {
                    return FuseOutHeader::error_frame(unique, -libc::EINVAL);
                };
                if self.is_read_only(dm) {
                    return FuseOutHeader::error_frame(unique, -libc::EROFS);
                }
                if (setattr_in.valid & FATTR_SIZE) != 0 {
                    match self.setattr_size(dm, hdr.nodeid, setattr_in.size) {
                        Ok(attr_out) => {
                            FuseOutHeader::ok_frame(unique, &attr_out.to_bytes_for(platform))
                        }
                        Err(e) => FuseOutHeader::error_frame(unique, e),
                    }
                } else {
                    match self.getattr(dm, hdr.nodeid) {
                        Ok(attr_out) => {
                            FuseOutHeader::ok_frame(unique, &attr_out.to_bytes_for(platform))
                        }
                        Err(e) => FuseOutHeader::error_frame(unique, e),
                    }
                }
            }
            FuseOpcode::Mkdir => {
                let Some(mkdir_in) = FuseMkdirIn::parse(payload) else {
                    return FuseOutHeader::error_frame(unique, -libc::EINVAL);
                };
                let name = match parse_fuse_name(&payload[FUSE_MKDIR_IN_SIZE..]) {
                    Ok(n) => n,
                    Err(e) => return FuseOutHeader::error_frame(unique, e),
                };
                match self.mkdir(dm, hdr.nodeid, name, mkdir_in.mode) {
                    Ok(entry) => FuseOutHeader::ok_frame(unique, &entry.to_bytes_for(platform)),
                    Err(e) => FuseOutHeader::error_frame(unique, e),
                }
            }
            FuseOpcode::Unlink => {
                let name = match parse_fuse_name(payload) {
                    Ok(n) => n,
                    Err(e) => return FuseOutHeader::error_frame(unique, e),
                };
                match self.unlink(dm, hdr.nodeid, name) {
                    Ok(()) => FuseOutHeader::ok_frame(unique, &[]),
                    Err(e) => FuseOutHeader::error_frame(unique, e),
                }
            }
            FuseOpcode::Rmdir => {
                let name = match parse_fuse_name(payload) {
                    Ok(n) => n,
                    Err(e) => return FuseOutHeader::error_frame(unique, e),
                };
                match self.rmdir(dm, hdr.nodeid, name) {
                    Ok(()) => FuseOutHeader::ok_frame(unique, &[]),
                    Err(e) => FuseOutHeader::error_frame(unique, e),
                }
            }
            FuseOpcode::Rename => {
                let Some(newdir) = read_u64_le(payload, 0) else {
                    return FuseOutHeader::error_frame(unique, -libc::EINVAL);
                };
                // On Linux, `fuse_rename_in` is 8 bytes (`newdir: u64`).
                // On macOS (macFUSE 4.x/5.x), `fuse_rename_in` may be 16 bytes
                // (`newdir: u64, flags: u32, padding: u32`) when `FUSE_RENAME` carries flags.
                let (flags, names_slice) = if platform == FuseAbiPlatform::MacOs
                    && payload.len() > 16
                    && payload[12..16] == [0, 0, 0, 0]
                    && (payload[8] == 0 || payload[9..12] == [0, 0, 0])
                {
                    (read_u32_le(payload, 8).unwrap_or(0), &payload[16..])
                } else {
                    (0, &payload[8..])
                };
                let (old_name, new_name) = match parse_fuse_two_names(names_slice) {
                    Ok(pair) => pair,
                    Err(e) => return FuseOutHeader::error_frame(unique, e),
                };
                match self.rename(dm, hdr.nodeid, old_name, newdir, new_name, flags) {
                    Ok(()) => FuseOutHeader::ok_frame(unique, &[]),
                    Err(e) => FuseOutHeader::error_frame(unique, e),
                }
            }
            FuseOpcode::Rename2 => {
                let (Some(newdir), Some(flags)) =
                    (read_u64_le(payload, 0), read_u32_le(payload, 8))
                else {
                    return FuseOutHeader::error_frame(unique, -libc::EINVAL);
                };
                if payload.len() < 16 {
                    return FuseOutHeader::error_frame(unique, -libc::EINVAL);
                }
                let (old_name, new_name) = match parse_fuse_two_names(&payload[16..]) {
                    Ok(pair) => pair,
                    Err(e) => return FuseOutHeader::error_frame(unique, e),
                };
                match self.rename(dm, hdr.nodeid, old_name, newdir, new_name, flags) {
                    Ok(()) => FuseOutHeader::ok_frame(unique, &[]),
                    Err(e) => FuseOutHeader::error_frame(unique, e),
                }
            }
            FuseOpcode::Open => {
                let Some(open_in) = FuseOpenIn::parse(payload) else {
                    return FuseOutHeader::error_frame(unique, -libc::EINVAL);
                };
                match self.open(dm, hdr.nodeid, open_in.flags) {
                    Ok(open_out) => FuseOutHeader::ok_frame(unique, &open_out.to_bytes()),
                    Err(e) => FuseOutHeader::error_frame(unique, e),
                }
            }
            FuseOpcode::Create => {
                let Some(create_in) = FuseCreateIn::parse(payload) else {
                    return FuseOutHeader::error_frame(unique, -libc::EINVAL);
                };
                let name = match parse_fuse_name(&payload[FUSE_CREATE_IN_SIZE..]) {
                    Ok(n) => n,
                    Err(e) => return FuseOutHeader::error_frame(unique, e),
                };
                match self.create(dm, hdr.nodeid, name, create_in.flags, create_in.mode) {
                    Ok((entry, open_out)) => {
                        let entry_bytes = entry.to_bytes_for(platform);
                        let mut body = Vec::with_capacity(entry_bytes.len() + FUSE_OPEN_OUT_SIZE);
                        body.extend_from_slice(&entry_bytes);
                        body.extend_from_slice(&open_out.to_bytes());
                        FuseOutHeader::ok_frame(unique, &body)
                    }
                    Err(e) => FuseOutHeader::error_frame(unique, e),
                }
            }
            FuseOpcode::Read => {
                let Some(read_in) = FuseReadIn::parse(payload) else {
                    return FuseOutHeader::error_frame(unique, -libc::EINVAL);
                };
                match self.read(dm, hdr.nodeid, read_in.fh, read_in.offset, read_in.size) {
                    Ok(data) => FuseOutHeader::ok_frame(unique, &data),
                    Err(e) => FuseOutHeader::error_frame(unique, e),
                }
            }
            FuseOpcode::Write => {
                let Some(write_in) = FuseWriteIn::parse(payload) else {
                    return FuseOutHeader::error_frame(unique, -libc::EINVAL);
                };
                let data_len = write_in.size as usize;
                let data_end = match FUSE_WRITE_IN_SIZE.checked_add(data_len) {
                    Some(end) if end <= payload.len() => end,
                    _ => return FuseOutHeader::error_frame(unique, -libc::EINVAL),
                };
                let data = &payload[FUSE_WRITE_IN_SIZE..data_end];
                match self.write(
                    dm,
                    hdr.nodeid,
                    write_in.fh,
                    write_in.offset,
                    data,
                    write_in.flags,
                ) {
                    Ok(written) => {
                        let out = FuseWriteOut {
                            size: written,
                            padding: 0,
                        };
                        FuseOutHeader::ok_frame(unique, &out.to_bytes())
                    }
                    Err(e) => FuseOutHeader::error_frame(unique, e),
                }
            }
            FuseOpcode::Flush => {
                let Some(flush_in) = FuseFlushIn::parse(payload) else {
                    return FuseOutHeader::error_frame(unique, -libc::EINVAL);
                };
                match self.flush(dm, hdr.nodeid, flush_in.fh) {
                    Ok(()) => FuseOutHeader::ok_frame(unique, &[]),
                    Err(e) => FuseOutHeader::error_frame(unique, e),
                }
            }
            FuseOpcode::Fsync | FuseOpcode::Fsyncdir => {
                let Some(fsync_in) = FuseFsyncIn::parse(payload) else {
                    return FuseOutHeader::error_frame(unique, -libc::EINVAL);
                };
                let datasync = (fsync_in.fsync_flags & 1) != 0;
                match self.fsync(dm, hdr.nodeid, fsync_in.fh, datasync) {
                    Ok(()) => FuseOutHeader::ok_frame(unique, &[]),
                    Err(e) => FuseOutHeader::error_frame(unique, e),
                }
            }
            FuseOpcode::Setxattr => {
                let Some((xattr_in, hdr_len)) = FuseSetXattrIn::parse_for(payload, platform)
                    .or_else(|| FuseSetXattrIn::parse_for(payload, FuseAbiPlatform::Linux))
                else {
                    return FuseOutHeader::error_frame(unique, -libc::EINVAL);
                };
                let rest = &payload[hdr_len..];
                let name = parse_fuse_name(rest).unwrap_or("");
                match self.setxattr(dm, hdr.nodeid, name, &[], xattr_in.flags) {
                    Ok(()) => FuseOutHeader::ok_frame(unique, &[]),
                    Err(e) => FuseOutHeader::error_frame(unique, e),
                }
            }
            FuseOpcode::Getxattr => {
                let Some((xattr_in, hdr_len)) = FuseGetXattrIn::parse_for(payload, platform)
                    .or_else(|| FuseGetXattrIn::parse_for(payload, FuseAbiPlatform::Linux))
                else {
                    return FuseOutHeader::error_frame(unique, -libc::EINVAL);
                };
                let name = parse_fuse_name(&payload[hdr_len..]).unwrap_or("");
                match self.getxattr(dm, hdr.nodeid, name, xattr_in.size) {
                    Ok(val) => FuseOutHeader::ok_frame(unique, &val),
                    Err(e) => FuseOutHeader::error_frame(unique, e),
                }
            }
            FuseOpcode::Listxattr => {
                let Some((xattr_in, _)) =
                    FuseGetXattrIn::parse_for(payload, FuseAbiPlatform::Linux)
                else {
                    return FuseOutHeader::error_frame(unique, -libc::EINVAL);
                };
                match self.listxattr(dm, hdr.nodeid, xattr_in.size) {
                    Ok(val) => FuseOutHeader::ok_frame(unique, &val),
                    Err(e) => FuseOutHeader::error_frame(unique, e),
                }
            }
            FuseOpcode::Removexattr => {
                let name = match parse_fuse_name(payload) {
                    Ok(n) => n,
                    Err(e) => return FuseOutHeader::error_frame(unique, e),
                };
                match self.removexattr(dm, hdr.nodeid, name) {
                    Ok(()) => FuseOutHeader::ok_frame(unique, &[]),
                    Err(e) => FuseOutHeader::error_frame(unique, e),
                }
            }
            FuseOpcode::Release => {
                let Some(rel_in) = FuseReleaseIn::parse(payload) else {
                    return FuseOutHeader::error_frame(unique, -libc::EINVAL);
                };
                match self.release(hdr.nodeid, rel_in.fh) {
                    Ok(()) => FuseOutHeader::ok_frame(unique, &[]),
                    Err(e) => FuseOutHeader::error_frame(unique, e),
                }
            }
            FuseOpcode::Opendir => {
                let Some(open_in) = FuseOpenIn::parse(payload) else {
                    return FuseOutHeader::error_frame(unique, -libc::EINVAL);
                };
                match self.opendir(dm, hdr.nodeid, open_in.flags) {
                    Ok(open_out) => FuseOutHeader::ok_frame(unique, &open_out.to_bytes()),
                    Err(e) => FuseOutHeader::error_frame(unique, e),
                }
            }
            FuseOpcode::Readdir => {
                let Some(read_in) = FuseReadIn::parse(payload) else {
                    return FuseOutHeader::error_frame(unique, -libc::EINVAL);
                };
                match self.readdir(dm, hdr.nodeid, read_in.fh, read_in.offset, read_in.size) {
                    Ok(entries) => {
                        let total_len: usize = entries.iter().map(FuseDirEntry::wire_len).sum();
                        let mut body = Vec::with_capacity(total_len);
                        for entry in &entries {
                            entry.encode_into(&mut body);
                        }
                        FuseOutHeader::ok_frame(unique, &body)
                    }
                    Err(e) => FuseOutHeader::error_frame(unique, e),
                }
            }
            FuseOpcode::Releasedir => {
                let Some(rel_in) = FuseReleaseIn::parse(payload) else {
                    return FuseOutHeader::error_frame(unique, -libc::EINVAL);
                };
                match self.releasedir(hdr.nodeid, rel_in.fh) {
                    Ok(()) => FuseOutHeader::ok_frame(unique, &[]),
                    Err(e) => FuseOutHeader::error_frame(unique, e),
                }
            }
            FuseOpcode::Statfs => match self.statfs(dm, hdr.nodeid) {
                Ok(statfs) => FuseOutHeader::ok_frame(unique, &statfs.to_bytes()),
                Err(e) => FuseOutHeader::error_frame(unique, e),
            },
            FuseOpcode::Access => {
                let Some(access_in) = FuseAccessIn::parse(payload) else {
                    return FuseOutHeader::error_frame(unique, -libc::EINVAL);
                };
                match self.access(dm, hdr.nodeid, access_in.mask) {
                    Ok(()) => FuseOutHeader::ok_frame(unique, &[]),
                    Err(e) => FuseOutHeader::error_frame(unique, e),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// High-level OifsFuseFs wrapper & transport runners
// ---------------------------------------------------------------------------

/// Pure-Rust FUSE Kernel ABI v7.28 filesystem engine backed by an OIFS [`DiskManager`].
pub struct OifsFuseFs {
    dm: Arc<DiskManager>,
    state: OifsFuseState,
}

impl OifsFuseFs {
    /// Creates a new FUSE filesystem instance taking ownership of a [`DiskManager`].
    pub fn new(dm: DiskManager, options: FuseMountOptions) -> Self {
        Self::from_arc(Arc::new(dm), options)
    }

    /// Creates a new FUSE filesystem instance sharing an `Arc<DiskManager>`.
    pub fn from_arc(dm: Arc<DiskManager>, options: FuseMountOptions) -> Self {
        Self {
            dm,
            state: OifsFuseState::new(options),
        }
    }

    /// Returns a reference to the underlying [`DiskManager`].
    #[inline]
    pub fn disk_manager(&self) -> &DiskManager {
        &self.dm
    }

    /// Returns the configured [`FuseMountOptions`].
    #[inline]
    pub fn options(&self) -> &FuseMountOptions {
        self.state.options()
    }

    /// Returns `true` if this FUSE filesystem enforces read-only semantics.
    #[inline]
    pub fn is_read_only(&self) -> bool {
        self.state.is_read_only(&self.dm)
    }

    /// Returns the number of currently open FUSE file and directory handles (`fh`).
    ///
    /// Opening thousands of FUSE handles stores entries in memory without opening any
    /// additional OS file descriptors beyond the single `.img` backing file.
    #[inline]
    pub fn open_handle_count(&self) -> usize {
        self.state.open_handle_count()
    }

    /// Returns OS-filtered mount option strings for `target_os` (`"linux"` or `"macos"`).
    pub fn build_mount_options_for_os(&self, target_os: &str) -> Vec<String> {
        self.options()
            .build_mount_options_for_os(target_os, self.is_read_only())
    }

    // --- High-level VFS API ---

    pub fn lookup(&self, parent: u64, name: &str) -> Result<FuseEntryOut, i32> {
        self.state.lookup(&self.dm, parent, name)
    }

    pub fn getattr(&self, nodeid: u64) -> Result<FuseAttrOut, i32> {
        self.state.getattr(&self.dm, nodeid)
    }

    pub fn setattr_size(&self, nodeid: u64, new_size: u64) -> Result<FuseAttrOut, i32> {
        self.state.setattr_size(&self.dm, nodeid, new_size)
    }

    pub fn mkdir(&self, parent: u64, name: &str, mode: u32) -> Result<FuseEntryOut, i32> {
        self.state.mkdir(&self.dm, parent, name, mode)
    }

    pub fn unlink(&self, parent: u64, name: &str) -> Result<(), i32> {
        self.state.unlink(&self.dm, parent, name)
    }

    pub fn rmdir(&self, parent: u64, name: &str) -> Result<(), i32> {
        self.state.rmdir(&self.dm, parent, name)
    }

    pub fn rename(
        &self,
        parent: u64,
        name: &str,
        newparent: u64,
        newname: &str,
        flags: u32,
    ) -> Result<(), i32> {
        self.state
            .rename(&self.dm, parent, name, newparent, newname, flags)
    }

    pub fn open(&self, nodeid: u64, flags: u32) -> Result<FuseOpenOut, i32> {
        self.state.open(&self.dm, nodeid, flags)
    }

    pub fn create(
        &self,
        parent: u64,
        name: &str,
        flags: u32,
        mode: u32,
    ) -> Result<(FuseEntryOut, FuseOpenOut), i32> {
        self.state.create(&self.dm, parent, name, flags, mode)
    }

    pub fn read(&self, nodeid: u64, fh: u64, offset: u64, size: u32) -> Result<Vec<u8>, i32> {
        self.state.read(&self.dm, nodeid, fh, offset, size)
    }

    pub fn write(
        &self,
        nodeid: u64,
        fh: u64,
        offset: u64,
        data: &[u8],
        flags: u32,
    ) -> Result<u32, i32> {
        self.state.write(&self.dm, nodeid, fh, offset, data, flags)
    }

    pub fn flush(&self, nodeid: u64, fh: u64) -> Result<(), i32> {
        self.state.flush(&self.dm, nodeid, fh)
    }

    pub fn fsync(&self, nodeid: u64, fh: u64, datasync: bool) -> Result<(), i32> {
        self.state.fsync(&self.dm, nodeid, fh, datasync)
    }

    pub fn release(&self, nodeid: u64, fh: u64) -> Result<(), i32> {
        self.state.release(nodeid, fh)
    }

    pub fn opendir(&self, nodeid: u64, flags: u32) -> Result<FuseOpenOut, i32> {
        self.state.opendir(&self.dm, nodeid, flags)
    }

    pub fn readdir(
        &self,
        nodeid: u64,
        fh: u64,
        offset: u64,
        max_bytes: u32,
    ) -> Result<Vec<FuseDirEntry>, i32> {
        self.state.readdir(&self.dm, nodeid, fh, offset, max_bytes)
    }

    pub fn releasedir(&self, nodeid: u64, fh: u64) -> Result<(), i32> {
        self.state.releasedir(nodeid, fh)
    }

    pub fn statfs(&self, nodeid: u64) -> Result<FuseKStatfs, i32> {
        self.state.statfs(&self.dm, nodeid)
    }

    pub fn access(&self, nodeid: u64, mask: u32) -> Result<(), i32> {
        self.state.access(&self.dm, nodeid, mask)
    }

    pub fn setxattr(&self, nodeid: u64, name: &str, value: &[u8], flags: u32) -> Result<(), i32> {
        self.state.setxattr(&self.dm, nodeid, name, value, flags)
    }

    pub fn getxattr(&self, nodeid: u64, name: &str, size: u32) -> Result<Vec<u8>, i32> {
        self.state.getxattr(&self.dm, nodeid, name, size)
    }

    pub fn listxattr(&self, nodeid: u64, size: u32) -> Result<Vec<u8>, i32> {
        self.state.listxattr(&self.dm, nodeid, size)
    }

    pub fn removexattr(&self, nodeid: u64, name: &str) -> Result<(), i32> {
        self.state.removexattr(&self.dm, nodeid, name)
    }

    // --- Binary FUSE Kernel Frame Dispatcher ---

    /// Parses and dispatches a single FUSE Kernel ABI v7.28 binary request frame using
    /// `self.options().abi_platform`.
    ///
    /// Returns the serialized `FuseOutHeader` + response body (at least 16 bytes), or an
    /// empty `Vec` for one-way notification opcodes (`FUSE_FORGET`, `FUSE_BATCH_FORGET`).
    /// Never panics on any input slice.
    pub fn dispatch_frame(&self, request: &[u8]) -> Vec<u8> {
        self.state.dispatch_frame(&self.dm, request)
    }

    /// Parses and dispatches a single FUSE binary request frame with an explicit
    /// [`FuseAbiPlatform`] (`Linux` 88-byte `FuseAttr` or `MacOs` 104-byte `FuseAttr`).
    pub fn dispatch_frame_for_platform(
        &self,
        request: &[u8],
        platform: FuseAbiPlatform,
    ) -> Vec<u8> {
        self.state
            .dispatch_frame_for_platform(&self.dm, request, platform)
    }

    // --- Packet Device, Stream & Kernel Mount Runners ---

    /// Serves FUSE requests on an atomic packet/message descriptor such as Linux `/dev/fuse`,
    /// macOS `/dev/macfuseN` / `/dev/osxfuseN`, or an `AF_UNIX` `SOCK_DGRAM` / `SOCK_SEQPACKET`
    /// socketpair.
    ///
    /// Each `libc::read(fd, ...)` receives one complete FUSE request frame (using a buffer of
    /// `FUSE_DEFAULT_MAX_WRITE + 4096` bytes) and each reply is written in a single atomic
    /// `libc::write(fd, ...)`. Retries on `EINTR`/`EAGAIN`, ignores `ENOENT` (interrupted
    /// kernel request), and exits cleanly with `Ok(())` on `ENODEV`, EOF (`0` bytes), or
    /// `FUSE_DESTROY`.
    pub fn serve_fuse_fd(&self, fd: RawFd) -> std::io::Result<()> {
        self.serve_fuse_fd_with_platform(fd, self.options().abi_platform)
    }

    /// Same as [`Self::serve_fuse_fd`], using an explicit [`FuseAbiPlatform`] for wire framing.
    pub fn serve_fuse_fd_with_platform(
        &self,
        fd: RawFd,
        platform: FuseAbiPlatform,
    ) -> std::io::Result<()> {
        let buf_len = (self.options().effective_max_write() as usize + 4096)
            .clamp(FUSE_LEGACY_MAX_WRITE as usize + 4096, FUSE_MAX_FRAME_LEN);
        let mut buf = vec![0u8; buf_len];
        loop {
            // SAFETY: `buf` is a valid writable buffer of `buf.len()` bytes.
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(libc::EINTR) | Some(libc::EAGAIN) | Some(libc::ENOENT) => continue,
                    Some(libc::ENODEV) | Some(libc::ECONNRESET) => return Ok(()),
                    _ => return Err(err),
                }
            }
            if n == 0 {
                return Ok(());
            }
            let req = &buf[..n as usize];
            let opcode = read_u32_le(req, 4).unwrap_or(0);
            let resp = self.dispatch_frame_for_platform(req, platform);
            if !resp.is_empty() {
                loop {
                    // SAFETY: `resp` is a valid readable slice of `resp.len()` bytes.
                    let wr = unsafe { libc::write(fd, resp.as_ptr().cast(), resp.len()) };
                    if wr < 0 {
                        let err = std::io::Error::last_os_error();
                        match err.raw_os_error() {
                            Some(libc::EINTR) => continue,
                            Some(libc::ENOENT) => break,
                            Some(libc::ENODEV) | Some(libc::EPIPE) => return Ok(()),
                            _ => return Err(err),
                        }
                    }
                    break;
                }
            }
            if opcode == FUSE_DESTROY {
                return Ok(());
            }
        }
    }

    /// Serves FUSE Kernel ABI v7.28 frames over a bidirectional Unix stream until clean EOF
    /// or a `FUSE_DESTROY` request is handled.
    pub fn serve_stream(&self, stream: &mut UnixStream) -> std::io::Result<()> {
        let mut hdr_buf = [0u8; FUSE_IN_HEADER_SIZE];
        loop {
            let mut read_so_far = 0usize;
            while read_so_far < FUSE_IN_HEADER_SIZE {
                match stream.read(&mut hdr_buf[read_so_far..]) {
                    Ok(0) if read_so_far == 0 => return Ok(()),
                    Ok(0) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "truncated FUSE request header",
                        ));
                    }
                    Ok(n) => read_so_far += n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }

            let Some(hdr) = FuseInHeader::parse_prefix(&hdr_buf) else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid FUSE request header length",
                ));
            };
            let frame_len = hdr.len as usize;
            if frame_len > FUSE_MAX_FRAME_LEN {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("FUSE request frame length {frame_len} exceeds maximum"),
                ));
            }

            let mut frame = vec![0u8; frame_len];
            frame[..FUSE_IN_HEADER_SIZE].copy_from_slice(&hdr_buf);
            if frame_len > FUSE_IN_HEADER_SIZE {
                stream.read_exact(&mut frame[FUSE_IN_HEADER_SIZE..])?;
            }

            let resp = self.dispatch_frame(&frame);
            if !resp.is_empty() {
                stream.write_all(&resp)?;
            }

            if hdr.opcode == FUSE_DESTROY {
                return Ok(());
            }
        }
    }

    /// Mounts this OIFS filesystem at `mountpoint` using the host OS kernel FUSE channel.
    ///
    /// - On **Linux**, spawns `fusermount3` (falling back to `fusermount`) with Linux-filtered
    ///   options and a `_FUSE_COMMFD` socketpair + `SCM_RIGHTS` (or direct `/dev/fuse` +
    ///   `mount(2)` when running privileged without `fusermount`), then serves kernel packets
    ///   with [`FuseAbiPlatform::Linux`].
    /// - On **macOS**, discovers `FUSE-T` (kext-free Apple Silicon NFSv4 backend), `macFUSE`,
    ///   and `osxfuse`, opens the first available `/dev/macfuse0..15` or `/dev/osxfuse0..15`
    ///   device, mounts with macOS-specific options (`default_permissions`, `noappledouble`,
    ///   `noapplexattr`, `local`, `volname=...`), and serves kernel packets with
    ///   [`FuseAbiPlatform::MacOs`]. If no macOS FUSE backend is installed, returns a clear
    ///   `io::Error` explaining how to install `FUSE-T` / `macFUSE` or use
    ///   [`Self::serve_fuse_fd`] / [`Self::serve_stream`] / [`Self::dispatch_frame`].
    pub fn mount<P: AsRef<Path>>(&self, mountpoint: P) -> std::io::Result<()> {
        let mountpoint = mountpoint.as_ref();
        if !mountpoint.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("mountpoint does not exist: {}", mountpoint.display()),
            ));
        }
        if !mountpoint.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("mountpoint is not a directory: {}", mountpoint.display()),
            ));
        }

        #[cfg(target_os = "linux")]
        {
            self.mount_linux(mountpoint)
        }

        #[cfg(target_os = "macos")]
        {
            self.mount_macos(mountpoint)
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "kernel FUSE mount is not supported on this platform; use serve_fuse_fd, serve_stream, or dispatch_frame",
            ))
        }
    }

    #[cfg(target_os = "macos")]
    fn mount_macos(&self, mountpoint: &Path) -> std::io::Result<()> {
        use std::os::unix::io::AsRawFd;

        let backend = discover_macos_fuse_backend();
        let opts = self.build_mount_options_for_os("macos").join(",");

        let Some(mount_helper) = (match backend {
            Some(MacOsFuseBackend::MacFuse(p)) | Some(MacOsFuseBackend::OsxFuse(p)) => Some(p),
            Some(MacOsFuseBackend::FuseT(p)) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    format!(
                        "FUSE-T detected at {}, which uses an NFSv4 userspace daemon rather than /dev/macfuse*",
                        p.display()
                    ),
                ));
            }
            None => None,
        }) else {
            // Try `fusermount` in PATH if provided (e.g. custom wrapper).
            if let Ok(fuse_fd) = self.spawn_fusermount_scm_rights(mountpoint, &opts) {
                let res =
                    self.serve_fuse_fd_with_platform(fuse_fd.as_raw_fd(), FuseAbiPlatform::MacOs);
                let _ = unmount(mountpoint);
                return res;
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!(
                    "kernel FUSE mount at {} failed (neither FUSE-T nor macFUSE/osxfuse is installed \
                     on this macOS host); install macFUSE, or use OifsFuseFs::serve_fuse_fd, \
                     OifsFuseFs::serve_stream, or OifsFuseFs::dispatch_frame for unprivileged userspace \
                     FUSE v7.28 access",
                    mountpoint.display()
                ),
            ));
        };

        // macFUSE (`mount_macfuse` / `mount_osxfuse`) uses `socketpair` + `SCM_RIGHTS` via
        // `_FUSE_CALL_BY_LIB=1`, `_FUSE_COMMFD=<fd>`, `_FUSE_COMMVERS=2`, `_FUSE_DAEMON_PATH=<exe>`,
        // and `-o <opts> <mountpoint>` (no positional device fd argument).
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `fds` is a valid two-element buffer for `socketpair(2)`.
        if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: both descriptors were freshly created by `socketpair(2)`.
        let parent_sock = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let child_sock = unsafe { OwnedFd::from_raw_fd(fds[1]) };

        let child_fd = child_sock.as_raw_fd();
        // SAFETY: clearing FD_CLOEXEC so `mount_macfuse` inherits `_FUSE_COMMFD`.
        unsafe {
            let flags = libc::fcntl(child_fd, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(child_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            }
        }

        let mut child = std::process::Command::new(&mount_helper)
            .env("_FUSE_CALL_BY_LIB", "1")
            .env("_FUSE_COMMFD", child_fd.to_string())
            .env("_FUSE_COMMVERS", "2")
            .env(
                "_FUSE_DAEMON_PATH",
                std::env::current_exe().unwrap_or_default(),
            )
            .arg("-o")
            .arg(&opts)
            .arg(mountpoint)
            .spawn()?;

        // Close parent's copy of child_sock before reading from parent_sock.
        drop(child_sock);

        let fuse_fd = match recv_fd_scm_rights(parent_sock.as_raw_fd()) {
            Ok(fd) => fd,
            Err(e) => {
                let st = child.wait();
                return match st {
                    Ok(status) if !status.success() => Err(std::io::Error::other(format!(
                        "{} exited with status {status} ({e})",
                        mount_helper.display()
                    ))),
                    _ => Err(e),
                };
            }
        };
        drop(parent_sock);

        // `mount_macfuse` calls `mount(2)` in the kernel *after* passing `fuse_fd` over
        // `_FUSE_COMMFD`, and `mount(2)` blocks until we answer `FUSE_INIT` on `fuse_fd`.
        // Reap `child` on a background thread so we enter `serve_fuse_fd_with_platform`
        // immediately without deadlocking on `child.wait()`.
        std::thread::spawn(move || {
            let _ = child.wait();
        });

        let res = self.serve_fuse_fd_with_platform(fuse_fd.as_raw_fd(), FuseAbiPlatform::MacOs);
        drop(fuse_fd);
        let _ = unmount(mountpoint);
        res
    }

    #[cfg(target_os = "linux")]
    fn mount_linux(&self, mountpoint: &Path) -> std::io::Result<()> {
        use std::os::unix::io::AsRawFd;

        let opts = self.build_mount_options_for_os("linux").join(",");

        // Primary path: unprivileged mount via `fusermount3` / `fusermount` + SCM_RIGHTS.
        let fuse_fd = match self.spawn_fusermount_scm_rights(mountpoint, &opts) {
            Ok(fd) => fd,
            Err(fm_err) => {
                // Fallback path: direct `/dev/fuse` open + `mount(2)` (for privileged/root containers).
                match self.mount_linux_direct_dev_fuse(mountpoint) {
                    Ok(fd) => fd,
                    Err(direct_err) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Unsupported,
                            format!(
                                "failed to mount FUSE at {} via fusermount ({fm_err}) and direct \
                                 /dev/fuse ({direct_err}); use OifsFuseFs::serve_fuse_fd, \
                                 serve_stream, or dispatch_frame",
                                mountpoint.display()
                            ),
                        ));
                    }
                }
            }
        };

        let res = self.serve_fuse_fd_with_platform(fuse_fd.as_raw_fd(), FuseAbiPlatform::Linux);
        drop(fuse_fd);
        let _ = unmount(mountpoint);
        res
    }

    #[cfg(target_os = "linux")]
    fn mount_linux_direct_dev_fuse(&self, mountpoint: &Path) -> std::io::Result<OwnedFd> {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::io::AsRawFd;

        let dev_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/fuse")?;
        let fd = dev_file.as_raw_fd();

        let source = CString::new("oifs").expect("static CString");
        let target = CString::new(mountpoint.as_os_str().as_bytes()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "mountpoint contains NUL")
        })?;
        let fstype = CString::new("fuse.oifs").expect("static CString");

        let mut data_str = format!(
            "fd={fd},rootmode=40000,user_id={},group_id={},default_permissions",
            self.options().uid,
            self.options().gid
        );
        if self.options().allow_other {
            data_str.push_str(",allow_other");
        }
        let data = CString::new(data_str).expect("valid mount data CString");

        let mut mount_flags = libc::MS_NOSUID | libc::MS_NODEV;
        if self.is_read_only() {
            mount_flags |= libc::MS_RDONLY;
        }

        // SAFETY: all CStrings are NUL-terminated and live across the `mount(2)` call.
        let rc = unsafe {
            libc::mount(
                source.as_ptr(),
                target.as_ptr(),
                fstype.as_ptr(),
                mount_flags,
                data.as_ptr().cast(),
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(OwnedFd::from(dev_file))
    }

    fn spawn_fusermount_scm_rights(
        &self,
        mountpoint: &Path,
        opts: &str,
    ) -> std::io::Result<OwnedFd> {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `fds` is a valid two-element buffer for `socketpair(2)`.
        if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Wrap both ends immediately so they are always closed on every exit path.
        // SAFETY: both descriptors were freshly created by `socketpair(2)`.
        let parent_sock = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let child_sock = unsafe { OwnedFd::from_raw_fd(fds[1]) };

        use std::os::unix::io::AsRawFd;
        let child_fd = child_sock.as_raw_fd();
        // SAFETY: clearing FD_CLOEXEC so `fusermount3`/`fusermount` inherits `_FUSE_COMMFD`.
        unsafe {
            let flags = libc::fcntl(child_fd, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(child_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            }
        }

        let mut child = std::process::Command::new("fusermount3")
            .env("_FUSE_COMMFD", child_fd.to_string())
            .arg("-o")
            .arg(opts)
            .arg("--")
            .arg(mountpoint)
            .spawn()
            .or_else(|_| {
                std::process::Command::new("fusermount")
                    .env("_FUSE_COMMFD", child_fd.to_string())
                    .arg("-o")
                    .arg(opts)
                    .arg("--")
                    .arg(mountpoint)
                    .spawn()
            })?;

        // Close parent's copy of child_sock before reading from parent_sock.
        drop(child_sock);

        let fuse_fd = recv_fd_scm_rights(parent_sock.as_raw_fd());
        drop(parent_sock);

        let status = child.wait()?;
        let fuse_fd = fuse_fd?;
        if !status.success() {
            return Err(std::io::Error::other(format!(
                "fusermount exited with status {status}"
            )));
        }
        Ok(fuse_fd)
    }
}

// ---------------------------------------------------------------------------
// Kani Formal Verification Proofs
// ---------------------------------------------------------------------------

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    /// Proves that `inode_to_nodeid` and `nodeid_to_inode` form a strict bijection for
    /// all valid inode IDs `< u64::MAX`, with OIFS root `0` mapped to `FUSE_ROOT_ID (1)`
    /// and invalid FUSE nodeid `0` rejected as `None`.
    #[kani::proof]
    fn proof_fuse_nodeid_inode_bijection_and_root_mapping() {
        assert_eq!(inode_to_nodeid(0), Some(FUSE_ROOT_ID));
        assert_eq!(nodeid_to_inode(0), None);
        assert_eq!(nodeid_to_inode(FUSE_ROOT_ID), Some(0));
        assert_eq!(inode_to_nodeid(u64::MAX), None);

        let inode_id: u64 = kani::any();
        kani::assume(inode_id < u64::MAX);
        let nodeid = inode_to_nodeid(inode_id).expect("valid inode_id must map to nodeid");
        assert!(nodeid >= FUSE_ROOT_ID);
        assert_eq!(nodeid_to_inode(nodeid), Some(inode_id));
    }

    /// Proves that 8-byte alignment rounding for `fuse_dirent` (`24 + name_len`) never
    /// overflows for any valid filename length (`<= 255`), always produces a multiple of 8,
    /// and is always at least `24 + name_len` and strictly within `< 24 + name_len + 8`.
    #[kani::proof]
    fn proof_fuse_dirent_align_and_record_bounds() {
        let name_len: usize = kani::any();
        kani::assume(name_len <= DirectoryEntry::MAX_FILENAME_LEN);
        let unaligned = FUSE_DIRENT_HEADER_SIZE + name_len;
        let aligned = fuse_dirent_align(unaligned);
        assert_eq!(aligned % 8, 0);
        assert!(aligned >= unaligned);
        assert!(aligned < unaligned + 8);
    }

    /// Proves that `FuseInHeader::parse` never panics on any arbitrary byte buffer up to
    /// 64 bytes, rejects all buffers shorter than 40 bytes, and rejects any buffer whose
    /// declared `len` field does not match `buf.len()`.
    #[kani::proof]
    fn proof_fuse_frame_header_parse_no_panic_and_bounds() {
        let buf: [u8; 64] = kani::any();
        let slice_len: usize = kani::any();
        kani::assume(slice_len <= buf.len());
        let slice = &buf[..slice_len];

        let parsed = FuseInHeader::parse(slice);
        if slice_len < FUSE_IN_HEADER_SIZE {
            assert!(parsed.is_none());
        } else {
            let declared_len = u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]);
            if (declared_len as usize) != slice_len {
                assert!(parsed.is_none());
            } else {
                let hdr = parsed.expect("exact-length 40+ byte frame must parse");
                assert_eq!(hdr.len as usize, slice_len);
            }
        }
    }
}
