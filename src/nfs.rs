//! Pure-Rust Local Loopback NFSv3 Server (ONC RPC v2 / XDR / MOUNT v3 / NFS v3).
//!
//! Provides zero-dependency, kext-free, reboot-free native OS mounting on macOS (via
//! built-in `mount_nfs`) and unprivileged Linux environments where `/dev/fuse` is
//! unavailable. Both `MOUNT v3` (program `100005`) and `NFS v3` (program `100003`) are
//! multiplexed over a single loopback TCP listener (`127.0.0.1:<port>`) with no
//! `rpcbind`/`portmap` daemon required.
//!
//! # Key Properties
//! - **Stateless 16-byte file handles**: `[fs_generation: u64 LE, inode_id: u64 LE]`,
//!   preserving OIFS's single OS file descriptor per image without per-file OS handles.
//! - **Panic-free bounded wire codec**: All XDR reads enforce 4-byte alignment and
//!   `NFS_MAX_FRAME_LEN` (2 MiB) caps; malformed or truncated RPC records return clean
//!   ONC RPC error replies (`GARBAGE_ARGS` / `PROC_UNAVAIL` / `PROG_MISMATCH`) instead of
//!   panicking.
//! - **Full OIFS parity**: Works across `StorageMode::Mmap` and `StorageMode::FileIo`,
//!   plain/journaled/encrypted images, compressed files, atomic `RENAME`, UTF-8 NFD->NFC
//!   normalization, macOS Finder metadata storm filtering, and strict `--read-only`
//!   immutable mounts (`NFS3ERR_ROFS`).

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::BLOCK_SIZE;
use crate::directory::DirectoryEntry;
use crate::disk::{CompressionMode, DiskManager, DiskManagerError};
use crate::fuse::{is_macos_metadata_filename, normalize_filename_nfc};
use crate::fx_hash::FxHashMap;
use crate::inode::{FileType, Inode};

// ---------------------------------------------------------------------------
// Wire Constants (RFC 5531, RFC 4506, RFC 1813)
// ---------------------------------------------------------------------------

/// Hard upper bound for any reassembled ONC RPC TCP record (2 MiB).
pub const NFS_MAX_FRAME_LEN: usize = 2 * 1024 * 1024;
/// Maximum payload bytes served per `READ` / `WRITE` RPC (128 KiB preferred, 1 MiB cap).
pub const NFS_PREFERRED_IO_SIZE: u32 = 128 * 1024;
/// Hard cap on a single `READ` / `WRITE` RPC payload.
pub const NFS_MAX_IO_SIZE: u32 = 1024 * 1024;
/// Preferred `READDIR` / `READDIRPLUS` transfer size.
pub const NFS_PREFERRED_READDIR_SIZE: u32 = 32 * 1024;
/// Fixed size of an OIFS stateless NFSv3 file handle in bytes.
pub const NFS_FILE_HANDLE_LEN: usize = 16;

// ONC RPC v2 constants
pub const RPC_VERSION_2: u32 = 2;
pub const RPC_MSG_CALL: u32 = 0;
pub const RPC_MSG_REPLY: u32 = 1;
pub const RPC_REPLY_MSG_ACCEPTED: u32 = 0;
pub const RPC_REPLY_MSG_DENIED: u32 = 1;
pub const RPC_ACCEPT_SUCCESS: u32 = 0;
pub const RPC_ACCEPT_PROG_UNAVAIL: u32 = 1;
pub const RPC_ACCEPT_PROG_MISMATCH: u32 = 2;
pub const RPC_ACCEPT_PROC_UNAVAIL: u32 = 3;
pub const RPC_ACCEPT_GARBAGE_ARGS: u32 = 4;
pub const RPC_REJECT_RPC_MISMATCH: u32 = 0;
pub const RPC_REJECT_AUTH_ERROR: u32 = 1;
pub const RPC_AUTH_NONE: u32 = 0;
pub const RPC_AUTH_SYS: u32 = 1;
pub const RPC_AUTH_BADCRED: u32 = 1;
pub const RPC_AUTH_REJECTEDCRED: u32 = 2;
pub const RPC_AUTH_TOOWEAK: u32 = 5;

// Program & version numbers
pub const NFS_PROGRAM: u32 = 100_003;
pub const NFS_VERSION_3: u32 = 3;
pub const MOUNT_PROGRAM: u32 = 100_005;
pub const MOUNT_VERSION_3: u32 = 3;

// MOUNT v3 procedures
pub const MNTPROC3_NULL: u32 = 0;
pub const MNTPROC3_MNT: u32 = 1;
pub const MNTPROC3_DUMP: u32 = 2;
pub const MNTPROC3_UMNT: u32 = 3;
pub const MNTPROC3_UMNTALL: u32 = 4;
pub const MNTPROC3_EXPORT: u32 = 5;

// MOUNT v3 status codes
pub const MNT3_OK: u32 = 0;
pub const MNT3ERR_NOENT: u32 = 2;
pub const MNT3ERR_ACCES: u32 = 13;

// NFS v3 procedures
pub const NFSPROC3_NULL: u32 = 0;
pub const NFSPROC3_GETATTR: u32 = 1;
pub const NFSPROC3_SETATTR: u32 = 2;
pub const NFSPROC3_LOOKUP: u32 = 3;
pub const NFSPROC3_ACCESS: u32 = 4;
pub const NFSPROC3_READLINK: u32 = 5;
pub const NFSPROC3_READ: u32 = 6;
pub const NFSPROC3_WRITE: u32 = 7;
pub const NFSPROC3_CREATE: u32 = 8;
pub const NFSPROC3_MKDIR: u32 = 9;
pub const NFSPROC3_SYMLINK: u32 = 10;
pub const NFSPROC3_MKNOD: u32 = 11;
pub const NFSPROC3_REMOVE: u32 = 12;
pub const NFSPROC3_RMDIR: u32 = 13;
pub const NFSPROC3_RENAME: u32 = 14;
pub const NFSPROC3_LINK: u32 = 15;
pub const NFSPROC3_READDIR: u32 = 16;
pub const NFSPROC3_READDIRPLUS: u32 = 17;
pub const NFSPROC3_FSSTAT: u32 = 18;
pub const NFSPROC3_FSINFO: u32 = 19;
pub const NFSPROC3_PATHCONF: u32 = 20;
pub const NFSPROC3_COMMIT: u32 = 21;

// NFS v3 status codes (RFC 1813 Section 2.6)
pub const NFS3_OK: u32 = 0;
pub const NFS3ERR_PERM: u32 = 1;
pub const NFS3ERR_NOENT: u32 = 2;
pub const NFS3ERR_IO: u32 = 5;
pub const NFS3ERR_ACCES: u32 = 13;
pub const NFS3ERR_EXIST: u32 = 17;
pub const NFS3ERR_NOTDIR: u32 = 20;
pub const NFS3ERR_ISDIR: u32 = 21;
pub const NFS3ERR_INVAL: u32 = 22;
pub const NFS3ERR_FBIG: u32 = 27;
pub const NFS3ERR_NOSPC: u32 = 28;
pub const NFS3ERR_ROFS: u32 = 30;
pub const NFS3ERR_NAMETOOLONG: u32 = 63;
pub const NFS3ERR_NOTEMPTY: u32 = 66;
pub const NFS3ERR_STALE: u32 = 70;
pub const NFS3ERR_BADHANDLE: u32 = 10001;
pub const NFS3ERR_NOTSUPP: u32 = 10004;

// NFS v3 file types (`ftype3`)
pub const NF3REG: u32 = 1;
pub const NF3DIR: u32 = 2;

// NFS v3 ACCESS bits
pub const ACCESS3_READ: u32 = 0x0001;
pub const ACCESS3_LOOKUP: u32 = 0x0002;
pub const ACCESS3_MODIFY: u32 = 0x0004;
pub const ACCESS3_EXTEND: u32 = 0x0008;
pub const ACCESS3_DELETE: u32 = 0x0010;
pub const ACCESS3_EXECUTE: u32 = 0x0020;

// NFS v3 WRITE stability levels (`stable_how`)
pub const UNSTABLE: u32 = 0;
pub const DATA_SYNC: u32 = 1;
pub const FILE_SYNC: u32 = 2;

// NFS v3 CREATE modes (`createmode3`)
pub const CREATE_UNCHECKED: u32 = 0;
pub const CREATE_GUARDED: u32 = 1;
pub const CREATE_EXCLUSIVE: u32 = 2;

// NFS v3 FSINFO properties
pub const FSF3_HOMOGENEOUS: u32 = 0x0008;
pub const FSF3_CANSETTIME: u32 = 0x0010;

// ---------------------------------------------------------------------------
// XDR Codec (RFC 4506)
// ---------------------------------------------------------------------------

/// Rounds `len` up to the next 4-byte XDR boundary, returning `None` on overflow.
#[inline]
pub const fn xdr_padded_len(len: usize) -> Option<usize> {
    match len.checked_add(3) {
        Some(v) => Some(v & !3),
        None => None,
    }
}

/// Panic-free, bounds-checked XDR decoder over a borrowed byte slice.
#[derive(Debug, Clone)]
pub struct XdrReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> XdrReader<'a> {
    #[inline]
    pub const fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    #[inline]
    pub const fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    #[inline]
    pub const fn position(&self) -> usize {
        self.pos
    }

    #[inline]
    pub fn read_u32(&mut self) -> Option<u32> {
        let end = self.pos.checked_add(4)?;
        let slice = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
    }

    #[inline]
    pub fn read_i32(&mut self) -> Option<i32> {
        self.read_u32().map(|v| v as i32)
    }

    #[inline]
    pub fn read_u64(&mut self) -> Option<u64> {
        let end = self.pos.checked_add(8)?;
        let slice = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(u64::from_be_bytes([
            slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
        ]))
    }

    #[inline]
    pub fn read_bool(&mut self) -> Option<bool> {
        match self.read_u32()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    /// Reads `len` fixed opaque bytes and advances past XDR 4-byte padding.
    #[inline]
    pub fn read_fixed_opaque(&mut self, len: usize) -> Option<&'a [u8]> {
        let padded = xdr_padded_len(len)?;
        let data_end = self.pos.checked_add(len)?;
        let padded_end = self.pos.checked_add(padded)?;
        if padded_end > self.buf.len() {
            return None;
        }
        let slice = &self.buf[self.pos..data_end];
        self.pos = padded_end;
        Some(slice)
    }

    /// Reads variable-length XDR opaque bytes (`u32` length + payload + 4-byte padding),
    /// rejecting any length exceeding `max_len`.
    #[inline]
    pub fn read_opaque(&mut self, max_len: usize) -> Option<&'a [u8]> {
        let len = self.read_u32()? as usize;
        if len > max_len {
            return None;
        }
        self.read_fixed_opaque(len)
    }

    /// Reads a UTF-8 XDR string bounded by `max_len`.
    #[inline]
    pub fn read_string(&mut self, max_len: usize) -> Option<&'a str> {
        let bytes = self.read_opaque(max_len)?;
        std::str::from_utf8(bytes).ok()
    }
}

/// Bounds-safe XDR encoder.
#[derive(Debug, Default, Clone)]
pub struct XdrWriter {
    buf: Vec<u8>,
}

impl XdrWriter {
    #[inline]
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    #[inline]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    #[inline]
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    #[inline]
    pub fn write_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    #[inline]
    pub fn write_u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    #[inline]
    pub fn write_bool(&mut self, v: bool) {
        self.write_u32(u32::from(v));
    }

    /// Writes fixed opaque bytes followed by 0..3 zero pad bytes for 4-byte alignment.
    #[inline]
    pub fn write_fixed_opaque(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        let pad = (4 - (data.len() & 3)) & 3;
        if pad > 0 {
            self.buf.extend_from_slice(&[0u8; 3][..pad]);
        }
    }

    /// Writes variable-length XDR opaque bytes (`u32` length + payload + 4-byte padding).
    #[inline]
    pub fn write_opaque(&mut self, data: &[u8]) {
        self.write_u32(data.len() as u32);
        self.write_fixed_opaque(data);
    }

    /// Writes an XDR UTF-8 string (`u32` length + UTF-8 bytes + 4-byte padding).
    #[inline]
    pub fn write_string(&mut self, s: &str) {
        self.write_opaque(s.as_bytes());
    }
}

// ---------------------------------------------------------------------------
// ONC RPC v2 Record Marking & Call Header
// ---------------------------------------------------------------------------

/// Parses the 4-byte ONC RPC TCP record-marking fragment header.
///
/// Returns `(is_last_fragment, fragment_length)`.
#[inline]
pub const fn parse_record_marking_header(hdr: [u8; 4]) -> (bool, usize) {
    let raw = u32::from_be_bytes(hdr);
    let is_last = (raw & 0x8000_0000) != 0;
    let frag_len = (raw & 0x7FFF_FFFF) as usize;
    (is_last, frag_len)
}

/// Maximum number of fragments accepted in a single ONC RPC TCP record.
const MAX_RPC_FRAGMENTS: usize = 64;
/// Maximum simultaneous NFSv3 TCP client connections accepted by [`OifsNfsServer`].
const MAX_CONCURRENT_NFS_CONNS: usize = 256;
/// Per-socket blocking I/O timeout for NFSv3 TCP client connections.
const NFS_CONN_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Reads one complete ONC RPC TCP record (reassembling multiple fragments) up to
/// [`NFS_MAX_FRAME_LEN`]. Returns `Ok(None)` on clean EOF at a record boundary.
pub fn read_rpc_record<R: Read>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut message = Vec::new();
    let mut any_fragment = false;
    let mut frag_count = 0usize;

    loop {
        let mut hdr = [0u8; 4];
        let mut read_hdr = 0usize;
        while read_hdr < 4 {
            match reader.read(&mut hdr[read_hdr..]) {
                Ok(0) if !any_fragment && read_hdr == 0 => return Ok(None),
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "truncated ONC RPC record marking header",
                    ));
                }
                Ok(n) => read_hdr += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        any_fragment = true;
        frag_count += 1;
        if frag_count > MAX_RPC_FRAGMENTS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ONC RPC record fragment count exceeds limit",
            ));
        }

        let (is_last, frag_len) = parse_record_marking_header(hdr);
        if frag_len == 0 && !is_last {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ONC RPC zero-length non-final fragment",
            ));
        }
        let new_total = message.len().checked_add(frag_len).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "ONC RPC record length overflow")
        })?;
        if new_total > NFS_MAX_FRAME_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "ONC RPC record size {new_total} exceeds NFS_MAX_FRAME_LEN ({NFS_MAX_FRAME_LEN})"
                ),
            ));
        }

        if frag_len > 0 {
            let old_len = message.len();
            message.resize(new_total, 0);
            reader.read_exact(&mut message[old_len..new_total])?;
        }

        if is_last {
            return Ok(Some(message));
        }
    }
}

/// Writes one ONC RPC reply payload wrapped in a single last-fragment record-marking frame.
pub fn write_rpc_record<W: Write>(writer: &mut W, payload: &[u8]) -> io::Result<()> {
    if payload.len() > NFS_MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ONC RPC reply exceeds NFS_MAX_FRAME_LEN",
        ));
    }
    let header = 0x8000_0000u32 | (payload.len() as u32);
    writer.write_all(&header.to_be_bytes())?;
    writer.write_all(payload)?;
    writer.flush()
}

/// Decoded ONC RPC v2 `CALL` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcCallHeader {
    pub xid: u32,
    pub rpcvers: u32,
    pub prog: u32,
    pub vers: u32,
    pub proc_num: u32,
    pub cred_flavor: u32,
    pub cred_uid: Option<u32>,
    pub cred_gid: Option<u32>,
}

/// Parses RFC 5531 `AUTH_SYS` opaque credentials and extracts `(uid, gid)`.
///
/// Layout: `stamp: u32`, `machinename: string<=255`, `uid: u32`, `gid: u32`, `gids: u32[<=16]`.
pub fn parse_auth_sys_uid_gid(cred_body: &[u8]) -> Option<(u32, u32)> {
    let mut r = XdrReader::new(cred_body);
    let _stamp = r.read_u32()?;
    let _machinename = r.read_opaque(255)?;
    let uid = r.read_u32()?;
    let gid = r.read_u32()?;
    let gids_len = r.read_u32()?;
    if gids_len > 16 {
        return None;
    }
    let gids_bytes = (gids_len as usize).checked_mul(4)?;
    let _ = r.read_fixed_opaque(gids_bytes)?;
    Some((uid, gid))
}

/// Returns `true` when an NFSv3 caller `caller_uid` is authorized to access a mount owned
/// by `owner_uid` (explicit `--allow-other`, exact owner UID, or host root UID 0).
#[inline]
pub const fn is_nfs_uid_authorized(caller_uid: u32, owner_uid: u32, allow_other: bool) -> bool {
    allow_other || caller_uid == owner_uid || caller_uid == 0
}

impl RpcCallHeader {
    /// Parses an ONC RPC `CALL` header from `reader`.
    ///
    /// Returns `None` if the record is truncated, is not `RPC_MSG_CALL`, or has
    /// oversized/malformed auth fields (> 400 bytes per RFC 5531).
    pub fn parse(reader: &mut XdrReader<'_>) -> Option<Self> {
        let xid = reader.read_u32()?;
        let mtype = reader.read_u32()?;
        if mtype != RPC_MSG_CALL {
            return None;
        }
        let rpcvers = reader.read_u32()?;
        let prog = reader.read_u32()?;
        let vers = reader.read_u32()?;
        let proc_num = reader.read_u32()?;
        let cred_flavor = reader.read_u32()?;
        let cred_body = reader.read_opaque(400)?;
        let (cred_uid, cred_gid) = if cred_flavor == RPC_AUTH_SYS {
            match parse_auth_sys_uid_gid(cred_body) {
                Some((uid, gid)) => (Some(uid), Some(gid)),
                None => (None, None),
            }
        } else {
            (None, None)
        };
        let _verf_flavor = reader.read_u32()?;
        let _verf_body = reader.read_opaque(400)?;
        Some(Self {
            xid,
            rpcvers,
            prog,
            vers,
            proc_num,
            cred_flavor,
            cred_uid,
            cred_gid,
        })
    }
}

fn encode_auth_error_reply(xid: u32, auth_stat: u32) -> Vec<u8> {
    let mut w = XdrWriter::with_capacity(20);
    w.write_u32(xid);
    w.write_u32(RPC_MSG_REPLY);
    w.write_u32(RPC_REPLY_MSG_DENIED);
    w.write_u32(RPC_REJECT_AUTH_ERROR);
    w.write_u32(auth_stat);
    w.into_bytes()
}

fn encode_nfs3_error_reply(xid: u32, proc_num: u32, status: u32) -> Vec<u8> {
    let mut w = begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS);
    match proc_num {
        NFSPROC3_GETATTR => {
            w.write_u32(status);
            w.into_bytes()
        }
        NFSPROC3_LOOKUP | NFSPROC3_ACCESS | NFSPROC3_READ | NFSPROC3_READDIR
        | NFSPROC3_READDIRPLUS | NFSPROC3_FSSTAT | NFSPROC3_FSINFO | NFSPROC3_PATHCONF
        | NFSPROC3_READLINK => {
            w.write_u32(status);
            w.write_bool(false);
            w.into_bytes()
        }
        NFSPROC3_SETATTR | NFSPROC3_WRITE | NFSPROC3_CREATE | NFSPROC3_MKDIR | NFSPROC3_REMOVE
        | NFSPROC3_RMDIR | NFSPROC3_COMMIT | NFSPROC3_SYMLINK | NFSPROC3_MKNOD => {
            w.write_u32(status);
            w.write_bool(false);
            w.write_bool(false);
            w.into_bytes()
        }
        NFSPROC3_RENAME => {
            w.write_u32(status);
            w.write_bool(false);
            w.write_bool(false);
            w.write_bool(false);
            w.write_bool(false);
            w.into_bytes()
        }
        NFSPROC3_LINK => {
            w.write_u32(status);
            w.write_bool(false);
            w.write_bool(false);
            w.write_bool(false);
            w.into_bytes()
        }
        _ => begin_accepted_reply(xid, RPC_ACCEPT_PROC_UNAVAIL).into_bytes(),
    }
}

fn begin_accepted_reply(xid: u32, accept_stat: u32) -> XdrWriter {
    let mut w = XdrWriter::with_capacity(128);
    w.write_u32(xid);
    w.write_u32(RPC_MSG_REPLY);
    w.write_u32(RPC_REPLY_MSG_ACCEPTED);
    // Reply verifier: AUTH_NONE with 0-byte body
    w.write_u32(RPC_AUTH_NONE);
    w.write_u32(0);
    w.write_u32(accept_stat);
    w
}

fn encode_rpc_mismatch_reply(xid: u32) -> Vec<u8> {
    let mut w = XdrWriter::with_capacity(32);
    w.write_u32(xid);
    w.write_u32(RPC_MSG_REPLY);
    w.write_u32(RPC_REPLY_MSG_DENIED);
    w.write_u32(RPC_REJECT_RPC_MISMATCH);
    w.write_u32(RPC_VERSION_2);
    w.write_u32(RPC_VERSION_2);
    w.into_bytes()
}

fn encode_prog_mismatch_reply(xid: u32, low: u32, high: u32) -> Vec<u8> {
    let mut w = begin_accepted_reply(xid, RPC_ACCEPT_PROG_MISMATCH);
    w.write_u32(low);
    w.write_u32(high);
    w.into_bytes()
}

// ---------------------------------------------------------------------------
// Stateless 16-Byte NFSv3 File Handle
// ---------------------------------------------------------------------------

/// Stateless 16-byte NFSv3 file handle mapping directly to an OIFS `inode_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NfsFileHandle {
    pub fs_generation: u64,
    pub inode_id: u64,
}

impl NfsFileHandle {
    #[inline]
    pub const fn new(fs_generation: u64, inode_id: u64) -> Self {
        Self {
            fs_generation,
            inode_id,
        }
    }

    #[inline]
    pub fn to_bytes(self) -> [u8; NFS_FILE_HANDLE_LEN] {
        let mut out = [0u8; NFS_FILE_HANDLE_LEN];
        out[0..8].copy_from_slice(&self.fs_generation.to_le_bytes());
        out[8..16].copy_from_slice(&self.inode_id.to_le_bytes());
        out
    }

    #[inline]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != NFS_FILE_HANDLE_LEN {
            return None;
        }
        let fs_generation = u64::from_le_bytes(bytes[0..8].try_into().ok()?);
        let inode_id = u64::from_le_bytes(bytes[8..16].try_into().ok()?);
        Some(Self {
            fs_generation,
            inode_id,
        })
    }
}

// ---------------------------------------------------------------------------
// Options & Error Mapping
// ---------------------------------------------------------------------------

/// Configuration options for [`OifsNfsFs`] and [`OifsNfsServer`].
#[derive(Debug, Clone)]
pub struct NfsMountOptions {
    /// Enforce read-only NFSv3 access (`NFS3ERR_ROFS` on all mutations).
    pub read_only: bool,
    /// Loopback TCP port (`0` selects an OS-assigned ephemeral port).
    pub port: u16,
    /// Compression policy used for NFS `WRITE` operations.
    pub compression_mode: CompressionMode,
    /// Reject macOS Finder metadata probes (`._*`, `.DS_Store`, `.Spotlight-V100`, etc.)
    /// with `NFS3ERR_NOENT` to prevent metadata storms.
    pub filter_macos_metadata: bool,
    /// Effective UID reported in `fattr3` and matched against `AUTH_SYS` caller UIDs.
    pub uid: u32,
    /// Effective GID reported in `fattr3`.
    pub gid: u32,
    /// Permit non-owner local UIDs to access the loopback NFSv3 server (`--allow-other`).
    pub allow_other: bool,
    /// Require `RPC_AUTH_SYS` credentials matching `uid` (or root UID 0) when `!allow_other`.
    pub require_auth_sys: bool,
}

impl Default for NfsMountOptions {
    fn default() -> Self {
        // SAFETY: `geteuid` and `getegid` are thread-safe POSIX getters with no preconditions.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        Self {
            read_only: false,
            port: 0,
            compression_mode: CompressionMode::Auto,
            filter_macos_metadata: true,
            uid,
            gid,
            allow_other: false,
            require_auth_sys: false,
        }
    }
}

fn disk_error_to_nfs3(err: &DiskManagerError) -> u32 {
    match err {
        DiskManagerError::ReadOnly => NFS3ERR_ROFS,
        DiskManagerError::Allocator(_) => NFS3ERR_NOSPC,
        DiskManagerError::PasswordRequired
        | DiskManagerError::DecryptionFailed
        | DiskManagerError::NotEncrypted
        | DiskManagerError::Encryption(_) => NFS3ERR_ACCES,
        DiskManagerError::Io(io_err) => match io_err.kind() {
            io::ErrorKind::NotFound => NFS3ERR_NOENT,
            io::ErrorKind::AlreadyExists => NFS3ERR_EXIST,
            io::ErrorKind::PermissionDenied => NFS3ERR_ACCES,
            io::ErrorKind::InvalidInput => {
                let msg = io_err.to_string();
                if msg.contains("Is a directory") {
                    NFS3ERR_ISDIR
                } else if msg.contains("Not a directory") || msg.contains("not a directory") {
                    NFS3ERR_NOTDIR
                } else if msg.contains("not empty") || msg.contains("Not empty") {
                    NFS3ERR_NOTEMPTY
                } else if msg.contains("too long") {
                    NFS3ERR_NAMETOOLONG
                } else {
                    NFS3ERR_INVAL
                }
            }
            _ => {
                let msg = io_err.to_string();
                if msg.contains("not found") || msg.contains("Not found") {
                    NFS3ERR_NOENT
                } else if msg.contains("already exists") {
                    NFS3ERR_EXIST
                } else if msg.contains("not empty") || msg.contains("Not empty") {
                    NFS3ERR_NOTEMPTY
                } else if msg.contains("Is a directory") {
                    NFS3ERR_ISDIR
                } else if msg.contains("Not a directory") || msg.contains("not a directory") {
                    NFS3ERR_NOTDIR
                } else if msg.contains("No space") || msg.contains("Out of space") {
                    NFS3ERR_NOSPC
                } else {
                    NFS3ERR_IO
                }
            }
        },
        _ => NFS3ERR_IO,
    }
}

// ---------------------------------------------------------------------------
// Core NFSv3 + MOUNT v3 Protocol Dispatcher
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct ParsedSattr3 {
    set_size: Option<u64>,
}

fn parse_sattr3(r: &mut XdrReader<'_>) -> Option<ParsedSattr3> {
    // mode
    if r.read_bool()? {
        let _mode = r.read_u32()?;
    }
    // uid
    if r.read_bool()? {
        let _uid = r.read_u32()?;
    }
    // gid
    if r.read_bool()? {
        let _gid = r.read_u32()?;
    }
    // size
    let set_size = if r.read_bool()? {
        Some(r.read_u64()?)
    } else {
        None
    };
    // atime (0=DONT_CHANGE, 1=SET_TO_SERVER_TIME, 2=SET_TO_CLIENT_TIME)
    match r.read_u32()? {
        0 | 1 => {}
        2 => {
            let _sec = r.read_u32()?;
            let _nsec = r.read_u32()?;
        }
        _ => return None,
    }
    // mtime
    match r.read_u32()? {
        0 | 1 => {}
        2 => {
            let _sec = r.read_u32()?;
            let _nsec = r.read_u32()?;
        }
        _ => return None,
    }
    Some(ParsedSattr3 { set_size })
}

fn generate_session_generation() -> u64 {
    #[cfg(not(kani))]
    {
        let mut bytes = [0u8; 8];
        chacha20poly1305::aead::rand_core::RngCore::fill_bytes(
            &mut chacha20poly1305::aead::OsRng,
            &mut bytes,
        );
        let v = u64::from_le_bytes(bytes);
        if v == 0 { 1 } else { v }
    }
    #[cfg(kani)]
    {
        0x4F49_4653_4E46_5333
    }
}

const NFS_FAST_PARENT_SLOTS: usize = 256;
const NFS_DIR_SNAPSHOT_CAP: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq)]
struct NfsDirSnapshotKey {
    dir_id: u64,
    write_gen: u64,
    size: u64,
    modified_at: u64,
}

type NfsSortedDirEntries = Arc<[(String, u64)]>;

#[inline]
fn pack_parent_pair(child: u64, parent: u64) -> Option<u64> {
    if child > 0 && child <= u32::MAX as u64 && parent <= u32::MAX as u64 {
        Some((child << 32) | parent)
    } else {
        None
    }
}

/// Pure-Rust NFSv3 + MOUNT v3 protocol dispatcher backed by a shared [`DiskManager`].
pub struct OifsNfsFs {
    dm: Arc<DiskManager>,
    options: NfsMountOptions,
    fs_generation: u64,
    write_verf: [u8; 8],
    mount_sealed: AtomicBool,
    parent_map: RwLock<FxHashMap<u64, u64>>,
    fast_parents: Box<[AtomicU64; NFS_FAST_PARENT_SLOTS]>,
    dir_snapshots: RwLock<VecDeque<(NfsDirSnapshotKey, NfsSortedDirEntries)>>,
}

impl OifsNfsFs {
    /// Wraps an existing `Arc<DiskManager>` as an NFSv3 filesystem dispatcher.
    pub fn from_arc(dm: Arc<DiskManager>, mut options: NfsMountOptions) -> Self {
        if dm.is_read_only() {
            options.read_only = true;
        }
        let fs_generation = generate_session_generation();
        let write_verf = fs_generation.to_be_bytes();
        let mut parents = FxHashMap::default();
        parents.insert(0, 0);
        let fast_vec: Vec<AtomicU64> = (0..NFS_FAST_PARENT_SLOTS)
            .map(|_| AtomicU64::new(0))
            .collect();
        let fast_parents: Box<[AtomicU64; NFS_FAST_PARENT_SLOTS]> = fast_vec
            .try_into()
            .unwrap_or_else(|_| unreachable!("fixed parent slot count"));
        Self {
            dm,
            options,
            fs_generation,
            write_verf,
            mount_sealed: AtomicBool::new(false),
            parent_map: RwLock::new(parents),
            fast_parents,
            dir_snapshots: RwLock::new(VecDeque::with_capacity(NFS_DIR_SNAPSHOT_CAP)),
        }
    }

    /// Closes the initial `MOUNT v3` handle-issuing phase after the OS kernel client
    /// completes `mount_nfs` / `mount -t nfs`, preventing unrelated local processes from
    /// minting root file handles over `127.0.0.1:<port>`.
    #[inline]
    pub fn seal_mount(&self) {
        self.mount_sealed.store(true, Ordering::Release);
    }

    /// Returns `true` after [`seal_mount`] has closed `MNTPROC3_MNT`.
    #[inline]
    pub fn is_mount_sealed(&self) -> bool {
        self.mount_sealed.load(Ordering::Acquire)
    }

    #[inline]
    pub fn is_read_only(&self) -> bool {
        self.options.read_only || self.dm.is_read_only()
    }

    #[inline]
    pub fn fs_generation(&self) -> u64 {
        self.fs_generation
    }

    #[inline]
    pub fn root_handle(&self) -> NfsFileHandle {
        NfsFileHandle::new(self.fs_generation, 0)
    }

    #[inline]
    pub fn disk_manager(&self) -> &Arc<DiskManager> {
        &self.dm
    }

    #[inline]
    fn handle_for(&self, inode_id: u64) -> NfsFileHandle {
        NfsFileHandle::new(self.fs_generation, inode_id)
    }

    fn record_parent(&self, child: u64, parent: u64) {
        if child == 0 {
            return;
        }
        if let Some(packed) = pack_parent_pair(child, parent) {
            self.fast_parents[(child as usize) & (NFS_FAST_PARENT_SLOTS - 1)]
                .store(packed, Ordering::Release);
        }
        let mut guard = crate::write_unpoisoned(&self.parent_map);
        guard.insert(child, parent);
    }

    fn forget_parent(&self, child: u64) {
        if child == 0 {
            return;
        }
        let slot = &self.fast_parents[(child as usize) & (NFS_FAST_PARENT_SLOTS - 1)];
        let cur = slot.load(Ordering::Relaxed);
        if cur != 0 && (cur >> 32) == child {
            slot.store(0, Ordering::Release);
        }
        let mut guard = crate::write_unpoisoned(&self.parent_map);
        guard.remove(&child);
    }

    fn parent_of(&self, inode_id: u64) -> u64 {
        if inode_id == 0 {
            return 0;
        }
        if inode_id <= u32::MAX as u64 {
            let packed = self.fast_parents[(inode_id as usize) & (NFS_FAST_PARENT_SLOTS - 1)]
                .load(Ordering::Acquire);
            if (packed >> 32) == inode_id {
                return packed & 0xFFFF_FFFF;
            }
        }
        let guard = crate::read_unpoisoned(&self.parent_map);
        guard.get(&inode_id).copied().unwrap_or(0)
    }

    fn decode_handle(&self, r: &mut XdrReader<'_>) -> Result<u64, u32> {
        let bytes = r.read_opaque(64).ok_or(NFS3ERR_BADHANDLE)?;
        let fh = NfsFileHandle::from_bytes(bytes).ok_or(NFS3ERR_BADHANDLE)?;
        if fh.fs_generation != self.fs_generation || !self.dm.is_inode_allocated(fh.inode_id) {
            return Err(NFS3ERR_STALE);
        }
        Ok(fh.inode_id)
    }

    fn write_fattr3(&self, w: &mut XdrWriter, inode_id: u64, inode: &Inode) {
        let ro = self.is_read_only();
        let (ftype, mode, nlink, size, used) = match inode.mode {
            FileType::Directory => (
                NF3DIR,
                if ro { 0o555u32 } else { 0o755u32 },
                2u32,
                BLOCK_SIZE as u64,
                BLOCK_SIZE as u64,
            ),
            FileType::File => {
                let phys = if inode.compressed_size > 0 {
                    inode.compressed_size
                } else {
                    inode.size
                };
                let rounded = phys.div_ceil(BLOCK_SIZE as u64) * (BLOCK_SIZE as u64);
                (
                    NF3REG,
                    if ro { 0o444u32 } else { 0o644u32 },
                    1u32,
                    inode.size,
                    rounded,
                )
            }
        };

        let mtime_sec = u32::try_from(inode.modified_at).unwrap_or(u32::MAX);
        let ctime_sec = u32::try_from(inode.created_at.max(inode.modified_at)).unwrap_or(u32::MAX);

        w.write_u32(ftype);
        w.write_u32(mode);
        w.write_u32(nlink);
        w.write_u32(self.options.uid);
        w.write_u32(self.options.gid);
        w.write_u64(size);
        w.write_u64(used);
        // specdata3 rdev
        w.write_u32(0);
        w.write_u32(0);
        // fsid (non-zero constant per mount)
        w.write_u64(self.fs_generation | 1);
        // fileid (must be >= 1 so root is 1, never 0)
        w.write_u64(inode_id.saturating_add(1));
        // atime, mtime, ctime
        w.write_u32(mtime_sec);
        w.write_u32(0);
        w.write_u32(mtime_sec);
        w.write_u32(0);
        w.write_u32(ctime_sec);
        w.write_u32(0);
    }

    fn write_post_op_attr(&self, w: &mut XdrWriter, inode_id: u64, inode: Option<&Inode>) {
        if let Some(ino) = inode {
            w.write_bool(true);
            self.write_fattr3(w, inode_id, ino);
        } else {
            w.write_bool(false);
        }
    }

    fn write_wcc_attr(w: &mut XdrWriter, inode: &Inode) {
        let size = match inode.mode {
            FileType::Directory => BLOCK_SIZE as u64,
            FileType::File => inode.size,
        };
        let mtime_sec = u32::try_from(inode.modified_at).unwrap_or(u32::MAX);
        let ctime_sec = u32::try_from(inode.created_at.max(inode.modified_at)).unwrap_or(u32::MAX);
        w.write_u64(size);
        w.write_u32(mtime_sec);
        w.write_u32(0);
        w.write_u32(ctime_sec);
        w.write_u32(0);
    }

    fn write_wcc_data(
        &self,
        w: &mut XdrWriter,
        inode_id: u64,
        pre: Option<&Inode>,
        post: Option<&Inode>,
    ) {
        if let Some(pre_ino) = pre {
            w.write_bool(true);
            Self::write_wcc_attr(w, pre_ino);
        } else {
            w.write_bool(false);
        }
        self.write_post_op_attr(w, inode_id, post);
    }

    fn write_post_op_fh(&self, w: &mut XdrWriter, inode_id: Option<u64>) {
        if let Some(id) = inode_id {
            w.write_bool(true);
            let bytes = self.handle_for(id).to_bytes();
            w.write_opaque(&bytes);
        } else {
            w.write_bool(false);
        }
    }

    fn read_inode_opt(&self, inode_id: u64) -> Option<Inode> {
        self.dm.read_allocated_inode(inode_id).ok()
    }

    /// Dispatches one complete ONC RPC v2 request message and returns the serialized
    /// ONC RPC v2 reply payload (without the 4-byte TCP record-marking header).
    ///
    /// Never panics on arbitrary/malformed input bytes.
    pub fn dispatch_rpc(&self, request: &[u8]) -> Vec<u8> {
        let mut r = XdrReader::new(request);
        let Some(hdr) = RpcCallHeader::parse(&mut r) else {
            // Try to salvage xid for a clean GARBAGE_ARGS reply if at least 4 bytes exist.
            let xid = if request.len() >= 4 {
                u32::from_be_bytes([request[0], request[1], request[2], request[3]])
            } else {
                0
            };
            return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes();
        };

        if hdr.rpcvers != RPC_VERSION_2 {
            return encode_rpc_mismatch_reply(hdr.xid);
        }

        if hdr.cred_flavor == RPC_AUTH_SYS && hdr.cred_uid.is_none() {
            return encode_auth_error_reply(hdr.xid, RPC_AUTH_BADCRED);
        }

        match hdr.prog {
            MOUNT_PROGRAM => {
                if hdr.vers != MOUNT_VERSION_3 {
                    return encode_prog_mismatch_reply(hdr.xid, MOUNT_VERSION_3, MOUNT_VERSION_3);
                }
                self.dispatch_mount_v3(&hdr, &mut r)
            }
            NFS_PROGRAM => {
                if hdr.vers != NFS_VERSION_3 {
                    return encode_prog_mismatch_reply(hdr.xid, NFS_VERSION_3, NFS_VERSION_3);
                }
                if hdr.proc_num != NFSPROC3_NULL {
                    if hdr.cred_flavor == RPC_AUTH_SYS {
                        let caller_uid = hdr.cred_uid.unwrap_or(u32::MAX);
                        if !is_nfs_uid_authorized(
                            caller_uid,
                            self.options.uid,
                            self.options.allow_other,
                        ) {
                            return encode_nfs3_error_reply(hdr.xid, hdr.proc_num, NFS3ERR_ACCES);
                        }
                    } else if self.options.require_auth_sys && !self.options.allow_other {
                        return encode_auth_error_reply(hdr.xid, RPC_AUTH_TOOWEAK);
                    }
                }
                self.dispatch_nfs_v3(hdr.xid, hdr.proc_num, &mut r)
            }
            _ => begin_accepted_reply(hdr.xid, RPC_ACCEPT_PROG_UNAVAIL).into_bytes(),
        }
    }

    // -----------------------------------------------------------------------
    // MOUNT v3 (Program 100005, Version 3)
    // -----------------------------------------------------------------------

    fn dispatch_mount_v3(&self, hdr: &RpcCallHeader, r: &mut XdrReader<'_>) -> Vec<u8> {
        let xid = hdr.xid;
        let proc_num = hdr.proc_num;
        match proc_num {
            MNTPROC3_NULL | MNTPROC3_UMNTALL => {
                begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS).into_bytes()
            }
            MNTPROC3_UMNT => {
                // Consume dirpath string if present
                let _ = r.read_string(1024);
                begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS).into_bytes()
            }
            MNTPROC3_DUMP => {
                let mut w = begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS);
                w.write_bool(false); // empty mountlist
                w.into_bytes()
            }
            MNTPROC3_EXPORT => {
                let mut w = begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS);
                if self.is_mount_sealed() {
                    w.write_bool(false);
                } else {
                    // Single export entry "/" open prior to mount sealing
                    w.write_bool(true);
                    w.write_string("/");
                    w.write_bool(false); // no group restrictions
                    w.write_bool(false); // end of export list
                }
                w.into_bytes()
            }
            MNTPROC3_MNT => {
                let Some(dirpath) = r.read_string(1024) else {
                    return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes();
                };
                if self.is_mount_sealed() {
                    let mut w = begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS);
                    w.write_u32(MNT3ERR_ACCES);
                    return w.into_bytes();
                }
                if hdr.cred_flavor == RPC_AUTH_SYS
                    && !is_nfs_uid_authorized(
                        hdr.cred_uid.unwrap_or(u32::MAX),
                        self.options.uid,
                        self.options.allow_other,
                    )
                {
                    let mut w = begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS);
                    w.write_u32(MNT3ERR_ACCES);
                    return w.into_bytes();
                }
                let mut w = begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS);
                let normalized = dirpath.trim_end_matches('/');
                if normalized.is_empty() || normalized == "/oifs" {
                    w.write_u32(MNT3_OK);
                    let root_bytes = self.root_handle().to_bytes();
                    w.write_opaque(&root_bytes);
                    if self.options.require_auth_sys && !self.options.allow_other {
                        w.write_u32(1);
                        w.write_u32(RPC_AUTH_SYS);
                    } else {
                        // Supported auth flavors: [AUTH_NONE, AUTH_SYS]
                        w.write_u32(2);
                        w.write_u32(RPC_AUTH_NONE);
                        w.write_u32(RPC_AUTH_SYS);
                    }
                } else {
                    w.write_u32(MNT3ERR_NOENT);
                }
                w.into_bytes()
            }
            _ => begin_accepted_reply(xid, RPC_ACCEPT_PROC_UNAVAIL).into_bytes(),
        }
    }

    // -----------------------------------------------------------------------
    // NFS v3 (Program 100003, Version 3)
    // -----------------------------------------------------------------------

    fn dispatch_nfs_v3(&self, xid: u32, proc_num: u32, r: &mut XdrReader<'_>) -> Vec<u8> {
        match proc_num {
            NFSPROC3_NULL => begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS).into_bytes(),
            NFSPROC3_GETATTR => self.nfs_getattr(xid, r),
            NFSPROC3_SETATTR => self.nfs_setattr(xid, r),
            NFSPROC3_LOOKUP => self.nfs_lookup(xid, r),
            NFSPROC3_ACCESS => self.nfs_access(xid, r),
            NFSPROC3_READ => self.nfs_read(xid, r),
            NFSPROC3_WRITE => self.nfs_write(xid, r),
            NFSPROC3_CREATE => self.nfs_create(xid, r),
            NFSPROC3_MKDIR => self.nfs_mkdir(xid, r),
            NFSPROC3_REMOVE => self.nfs_remove(xid, r),
            NFSPROC3_RMDIR => self.nfs_rmdir(xid, r),
            NFSPROC3_RENAME => self.nfs_rename(xid, r),
            NFSPROC3_READDIR => self.nfs_readdir(xid, r),
            NFSPROC3_READDIRPLUS => self.nfs_readdirplus(xid, r),
            NFSPROC3_FSSTAT => self.nfs_fsstat(xid, r),
            NFSPROC3_FSINFO => self.nfs_fsinfo(xid, r),
            NFSPROC3_PATHCONF => self.nfs_pathconf(xid, r),
            NFSPROC3_COMMIT => self.nfs_commit(xid, r),
            NFSPROC3_READLINK => {
                let mut w = begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS);
                w.write_u32(NFS3ERR_NOTSUPP);
                w.write_bool(false); // post_op_attr
                w.into_bytes()
            }
            NFSPROC3_SYMLINK | NFSPROC3_MKNOD => {
                let mut w = begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS);
                w.write_u32(NFS3ERR_NOTSUPP);
                w.write_bool(false); // pre_op_attr
                w.write_bool(false); // post_op_attr
                w.into_bytes()
            }
            NFSPROC3_LINK => {
                let mut w = begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS);
                w.write_u32(NFS3ERR_NOTSUPP);
                w.write_bool(false); // file_attributes
                w.write_bool(false); // linkdir_wcc pre
                w.write_bool(false); // linkdir_wcc post
                w.into_bytes()
            }
            _ => begin_accepted_reply(xid, RPC_ACCEPT_PROC_UNAVAIL).into_bytes(),
        }
    }

    fn nfs_getattr(&self, xid: u32, r: &mut XdrReader<'_>) -> Vec<u8> {
        let mut w = begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS);
        let inode_id = match self.decode_handle(r) {
            Ok(id) => id,
            Err(status) => {
                w.write_u32(status);
                return w.into_bytes();
            }
        };
        match self.dm.read_allocated_inode(inode_id) {
            Ok(inode) => {
                w.write_u32(NFS3_OK);
                self.write_fattr3(&mut w, inode_id, &inode);
            }
            Err(DiskManagerError::Io(ref e)) if e.kind() == std::io::ErrorKind::NotFound => {
                w.write_u32(NFS3ERR_STALE);
            }
            Err(e) => w.write_u32(disk_error_to_nfs3(&e)),
        }
        w.into_bytes()
    }

    fn nfs_setattr(&self, xid: u32, r: &mut XdrReader<'_>) -> Vec<u8> {
        let mut w = begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS);
        let inode_id = match self.decode_handle(r) {
            Ok(id) => id,
            Err(status) => {
                w.write_u32(status);
                self.write_wcc_data(&mut w, 0, None, None);
                return w.into_bytes();
            }
        };
        let Some(sattr) = parse_sattr3(r) else {
            return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes();
        };
        // sattrguard3
        let Some(guard_check) = r.read_bool() else {
            return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes();
        };
        if guard_check && (r.read_u32().is_none() || r.read_u32().is_none()) {
            return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes();
        }

        let pre = self.read_inode_opt(inode_id);
        if self.is_read_only() {
            w.write_u32(NFS3ERR_ROFS);
            self.write_wcc_data(&mut w, inode_id, pre.as_ref(), pre.as_ref());
            return w.into_bytes();
        }
        if pre.is_none() {
            w.write_u32(NFS3ERR_STALE);
            self.write_wcc_data(&mut w, inode_id, None, None);
            return w.into_bytes();
        }

        if let Some(new_size) = sattr.set_size
            && let Err(e) = self.dm.truncate(inode_id, new_size)
        {
            let post = self.read_inode_opt(inode_id);
            w.write_u32(disk_error_to_nfs3(&e));
            self.write_wcc_data(&mut w, inode_id, pre.as_ref(), post.as_ref());
            return w.into_bytes();
        }

        let post = self.read_inode_opt(inode_id);
        if post.is_none() {
            w.write_u32(NFS3ERR_STALE);
            self.write_wcc_data(&mut w, inode_id, pre.as_ref(), None);
            return w.into_bytes();
        }
        w.write_u32(NFS3_OK);
        self.write_wcc_data(&mut w, inode_id, pre.as_ref(), post.as_ref());
        w.into_bytes()
    }

    fn nfs_lookup(&self, xid: u32, r: &mut XdrReader<'_>) -> Vec<u8> {
        let mut w = begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS);
        let dir_id = match self.decode_handle(r) {
            Ok(id) => id,
            Err(status) => {
                w.write_u32(status);
                w.write_bool(false);
                return w.into_bytes();
            }
        };
        let Some(raw_name) = r.read_string(4096) else {
            return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes();
        };

        let dir_ino = self.read_inode_opt(dir_id);
        let Some(ref d) = dir_ino else {
            w.write_u32(NFS3ERR_STALE);
            w.write_bool(false);
            return w.into_bytes();
        };
        if d.mode != FileType::Directory {
            w.write_u32(NFS3ERR_NOTDIR);
            self.write_post_op_attr(&mut w, dir_id, dir_ino.as_ref());
            return w.into_bytes();
        }

        if raw_name == "." {
            w.write_u32(NFS3_OK);
            let fh = self.handle_for(dir_id).to_bytes();
            w.write_opaque(&fh);
            self.write_post_op_attr(&mut w, dir_id, dir_ino.as_ref());
            self.write_post_op_attr(&mut w, dir_id, dir_ino.as_ref());
            return w.into_bytes();
        }
        if raw_name == ".." {
            let parent_id = self.parent_of(dir_id);
            let parent_ino = self.read_inode_opt(parent_id);
            w.write_u32(NFS3_OK);
            let fh = self.handle_for(parent_id).to_bytes();
            w.write_opaque(&fh);
            self.write_post_op_attr(&mut w, parent_id, parent_ino.as_ref());
            self.write_post_op_attr(&mut w, dir_id, dir_ino.as_ref());
            return w.into_bytes();
        }

        let name = normalize_filename_nfc(raw_name);
        if name.len() > DirectoryEntry::MAX_FILENAME_LEN {
            w.write_u32(NFS3ERR_NAMETOOLONG);
            self.write_post_op_attr(&mut w, dir_id, dir_ino.as_ref());
            return w.into_bytes();
        }
        if self.options.filter_macos_metadata && is_macos_metadata_filename(&name) {
            w.write_u32(NFS3ERR_NOENT);
            self.write_post_op_attr(&mut w, dir_id, dir_ino.as_ref());
            return w.into_bytes();
        }

        match self.dm.lookup(dir_id, &name) {
            Ok(child_id) => {
                self.record_parent(child_id, dir_id);
                let child_ino = self.read_inode_opt(child_id);
                w.write_u32(NFS3_OK);
                let fh = self.handle_for(child_id).to_bytes();
                w.write_opaque(&fh);
                self.write_post_op_attr(&mut w, child_id, child_ino.as_ref());
                self.write_post_op_attr(&mut w, dir_id, dir_ino.as_ref());
            }
            Err(e) => {
                w.write_u32(disk_error_to_nfs3(&e));
                self.write_post_op_attr(&mut w, dir_id, dir_ino.as_ref());
            }
        }
        w.into_bytes()
    }

    fn nfs_access(&self, xid: u32, r: &mut XdrReader<'_>) -> Vec<u8> {
        let mut w = begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS);
        let inode_id = match self.decode_handle(r) {
            Ok(id) => id,
            Err(status) => {
                w.write_u32(status);
                w.write_bool(false);
                return w.into_bytes();
            }
        };
        let Some(requested) = r.read_u32() else {
            return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes();
        };

        let Some(inode) = self.read_inode_opt(inode_id) else {
            w.write_u32(NFS3ERR_STALE);
            w.write_bool(false);
            return w.into_bytes();
        };

        let mut allowed = match inode.mode {
            FileType::Directory => {
                ACCESS3_READ
                    | ACCESS3_LOOKUP
                    | ACCESS3_MODIFY
                    | ACCESS3_EXTEND
                    | ACCESS3_DELETE
                    | ACCESS3_EXECUTE
            }
            FileType::File => ACCESS3_READ | ACCESS3_MODIFY | ACCESS3_EXTEND | ACCESS3_EXECUTE,
        };
        if self.is_read_only() {
            allowed &= !(ACCESS3_MODIFY | ACCESS3_EXTEND | ACCESS3_DELETE);
        }

        w.write_u32(NFS3_OK);
        self.write_post_op_attr(&mut w, inode_id, Some(&inode));
        w.write_u32(requested & allowed);
        w.into_bytes()
    }

    fn nfs_read(&self, xid: u32, r: &mut XdrReader<'_>) -> Vec<u8> {
        let mut w = begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS);
        let inode_id = match self.decode_handle(r) {
            Ok(id) => id,
            Err(status) => {
                w.write_u32(status);
                w.write_bool(false);
                return w.into_bytes();
            }
        };
        let (Some(offset), Some(count)) = (r.read_u64(), r.read_u32()) else {
            return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes();
        };

        let inode = match self.dm.read_allocated_inode(inode_id) {
            Ok(ino) => ino,
            Err(DiskManagerError::Io(ref e)) if e.kind() == std::io::ErrorKind::NotFound => {
                w.write_u32(NFS3ERR_STALE);
                w.write_bool(false);
                return w.into_bytes();
            }
            Err(e) => {
                w.write_u32(disk_error_to_nfs3(&e));
                w.write_bool(false);
                return w.into_bytes();
            }
        };
        if inode.mode == FileType::Directory {
            w.write_u32(NFS3ERR_ISDIR);
            self.write_post_op_attr(&mut w, inode_id, Some(&inode));
            return w.into_bytes();
        }

        let capped = (count.min(NFS_MAX_IO_SIZE)) as usize;
        let available = inode.size.saturating_sub(offset).min(capped as u64) as usize;
        let mut buf = vec![0u8; available];

        let n = if available == 0 {
            0
        } else {
            match self.dm.read_at(inode_id, offset, &mut buf) {
                Ok(n) => n,
                Err(e) => {
                    w.write_u32(disk_error_to_nfs3(&e));
                    self.write_post_op_attr(&mut w, inode_id, Some(&inode));
                    return w.into_bytes();
                }
            }
        };
        buf.truncate(n);
        let eof = n < available || offset.saturating_add(n as u64) >= inode.size;

        w.write_u32(NFS3_OK);
        self.write_post_op_attr(&mut w, inode_id, Some(&inode));
        w.write_u32(n as u32);
        w.write_bool(eof);
        w.write_opaque(&buf);
        w.into_bytes()
    }

    fn nfs_write(&self, xid: u32, r: &mut XdrReader<'_>) -> Vec<u8> {
        let mut w = begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS);
        let inode_id = match self.decode_handle(r) {
            Ok(id) => id,
            Err(status) => {
                w.write_u32(status);
                self.write_wcc_data(&mut w, 0, None, None);
                return w.into_bytes();
            }
        };
        let (Some(offset), Some(count), Some(stable)) = (r.read_u64(), r.read_u32(), r.read_u32())
        else {
            return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes();
        };
        let Some(data) = r.read_opaque(NFS_MAX_IO_SIZE as usize) else {
            return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes();
        };

        let pre = self.read_inode_opt(inode_id);
        if self.is_read_only() {
            w.write_u32(NFS3ERR_ROFS);
            self.write_wcc_data(&mut w, inode_id, pre.as_ref(), pre.as_ref());
            return w.into_bytes();
        }
        if pre.is_none() {
            w.write_u32(NFS3ERR_STALE);
            self.write_wcc_data(&mut w, inode_id, None, None);
            return w.into_bytes();
        }
        if let Some(ref ino) = pre
            && ino.mode == FileType::Directory
        {
            w.write_u32(NFS3ERR_ISDIR);
            self.write_wcc_data(&mut w, inode_id, pre.as_ref(), pre.as_ref());
            return w.into_bytes();
        }

        let write_len = (count as usize).min(data.len());
        let slice = &data[..write_len];

        if let Err(e) = self
            .dm
            .write_data(inode_id, offset, slice, self.options.compression_mode)
        {
            let post = self.read_inode_opt(inode_id);
            w.write_u32(disk_error_to_nfs3(&e));
            self.write_wcc_data(&mut w, inode_id, pre.as_ref(), post.as_ref());
            return w.into_bytes();
        }

        let committed = if stable == DATA_SYNC || stable == FILE_SYNC {
            if let Err(e) = self.dm.flush() {
                let post = self.read_inode_opt(inode_id);
                w.write_u32(disk_error_to_nfs3(&e));
                self.write_wcc_data(&mut w, inode_id, pre.as_ref(), post.as_ref());
                return w.into_bytes();
            }
            FILE_SYNC
        } else {
            UNSTABLE
        };

        let post = self.read_inode_opt(inode_id);
        w.write_u32(NFS3_OK);
        self.write_wcc_data(&mut w, inode_id, pre.as_ref(), post.as_ref());
        w.write_u32(write_len as u32);
        w.write_u32(committed);
        w.write_fixed_opaque(&self.write_verf);
        w.into_bytes()
    }

    fn nfs_create(&self, xid: u32, r: &mut XdrReader<'_>) -> Vec<u8> {
        let mut w = begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS);
        let dir_id = match self.decode_handle(r) {
            Ok(id) => id,
            Err(status) => {
                w.write_u32(status);
                self.write_wcc_data(&mut w, 0, None, None);
                return w.into_bytes();
            }
        };
        let Some(raw_name) = r.read_string(4096) else {
            return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes();
        };
        let Some(create_mode) = r.read_u32() else {
            return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes();
        };
        let sattr = match create_mode {
            CREATE_UNCHECKED | CREATE_GUARDED => {
                let Some(s) = parse_sattr3(r) else {
                    return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes();
                };
                s
            }
            CREATE_EXCLUSIVE => {
                if r.read_fixed_opaque(8).is_none() {
                    return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes();
                }
                ParsedSattr3::default()
            }
            _ => return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes(),
        };

        let pre_dir = self.read_inode_opt(dir_id);
        if self.is_read_only() {
            w.write_u32(NFS3ERR_ROFS);
            self.write_wcc_data(&mut w, dir_id, pre_dir.as_ref(), pre_dir.as_ref());
            return w.into_bytes();
        }

        let name = normalize_filename_nfc(raw_name);
        if name.len() > DirectoryEntry::MAX_FILENAME_LEN {
            w.write_u32(NFS3ERR_NAMETOOLONG);
            self.write_wcc_data(&mut w, dir_id, pre_dir.as_ref(), pre_dir.as_ref());
            return w.into_bytes();
        }
        if self.options.filter_macos_metadata && is_macos_metadata_filename(&name) {
            w.write_u32(NFS3ERR_NOENT);
            self.write_wcc_data(&mut w, dir_id, pre_dir.as_ref(), pre_dir.as_ref());
            return w.into_bytes();
        }

        let created_id = match self.dm.lookup(dir_id, &name) {
            Ok(existing_id) => {
                if create_mode == CREATE_GUARDED || create_mode == CREATE_EXCLUSIVE {
                    w.write_u32(NFS3ERR_EXIST);
                    self.write_wcc_data(&mut w, dir_id, pre_dir.as_ref(), pre_dir.as_ref());
                    return w.into_bytes();
                }
                if let Some(new_size) = sattr.set_size
                    && let Err(e) = self.dm.truncate(existing_id, new_size)
                {
                    let post_dir = self.read_inode_opt(dir_id);
                    w.write_u32(disk_error_to_nfs3(&e));
                    self.write_wcc_data(&mut w, dir_id, pre_dir.as_ref(), post_dir.as_ref());
                    return w.into_bytes();
                }
                existing_id
            }
            Err(_) => match self.dm.create_file(dir_id, &name) {
                Ok(new_id) => {
                    if let Some(new_size) = sattr.set_size
                        && new_size > 0
                        && let Err(e) = self.dm.truncate(new_id, new_size)
                    {
                        let post_dir = self.read_inode_opt(dir_id);
                        w.write_u32(disk_error_to_nfs3(&e));
                        self.write_wcc_data(&mut w, dir_id, pre_dir.as_ref(), post_dir.as_ref());
                        return w.into_bytes();
                    }
                    new_id
                }
                Err(e) => {
                    let post_dir = self.read_inode_opt(dir_id);
                    w.write_u32(disk_error_to_nfs3(&e));
                    self.write_wcc_data(&mut w, dir_id, pre_dir.as_ref(), post_dir.as_ref());
                    return w.into_bytes();
                }
            },
        };

        self.record_parent(created_id, dir_id);
        let obj_ino = self.read_inode_opt(created_id);
        let post_dir = self.read_inode_opt(dir_id);

        w.write_u32(NFS3_OK);
        self.write_post_op_fh(&mut w, Some(created_id));
        self.write_post_op_attr(&mut w, created_id, obj_ino.as_ref());
        self.write_wcc_data(&mut w, dir_id, pre_dir.as_ref(), post_dir.as_ref());
        w.into_bytes()
    }

    fn nfs_mkdir(&self, xid: u32, r: &mut XdrReader<'_>) -> Vec<u8> {
        let mut w = begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS);
        let dir_id = match self.decode_handle(r) {
            Ok(id) => id,
            Err(status) => {
                w.write_u32(status);
                self.write_wcc_data(&mut w, 0, None, None);
                return w.into_bytes();
            }
        };
        let Some(raw_name) = r.read_string(4096) else {
            return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes();
        };
        if parse_sattr3(r).is_none() {
            return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes();
        }

        let pre_dir = self.read_inode_opt(dir_id);
        if self.is_read_only() {
            w.write_u32(NFS3ERR_ROFS);
            self.write_wcc_data(&mut w, dir_id, pre_dir.as_ref(), pre_dir.as_ref());
            return w.into_bytes();
        }

        let name = normalize_filename_nfc(raw_name);
        if name.len() > DirectoryEntry::MAX_FILENAME_LEN {
            w.write_u32(NFS3ERR_NAMETOOLONG);
            self.write_wcc_data(&mut w, dir_id, pre_dir.as_ref(), pre_dir.as_ref());
            return w.into_bytes();
        }

        match self.dm.create_directory(dir_id, &name) {
            Ok(new_id) => {
                self.record_parent(new_id, dir_id);
                let obj_ino = self.read_inode_opt(new_id);
                let post_dir = self.read_inode_opt(dir_id);
                w.write_u32(NFS3_OK);
                self.write_post_op_fh(&mut w, Some(new_id));
                self.write_post_op_attr(&mut w, new_id, obj_ino.as_ref());
                self.write_wcc_data(&mut w, dir_id, pre_dir.as_ref(), post_dir.as_ref());
            }
            Err(e) => {
                let post_dir = self.read_inode_opt(dir_id);
                w.write_u32(disk_error_to_nfs3(&e));
                self.write_wcc_data(&mut w, dir_id, pre_dir.as_ref(), post_dir.as_ref());
            }
        }
        w.into_bytes()
    }

    fn nfs_remove(&self, xid: u32, r: &mut XdrReader<'_>) -> Vec<u8> {
        let mut w = begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS);
        let dir_id = match self.decode_handle(r) {
            Ok(id) => id,
            Err(status) => {
                w.write_u32(status);
                self.write_wcc_data(&mut w, 0, None, None);
                return w.into_bytes();
            }
        };
        let Some(raw_name) = r.read_string(4096) else {
            return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes();
        };

        let pre_dir = self.read_inode_opt(dir_id);
        if self.is_read_only() {
            w.write_u32(NFS3ERR_ROFS);
            self.write_wcc_data(&mut w, dir_id, pre_dir.as_ref(), pre_dir.as_ref());
            return w.into_bytes();
        }

        let name = normalize_filename_nfc(raw_name);
        let child_id = match self.dm.lookup(dir_id, &name) {
            Ok(id) => {
                if let Ok(ino) = self.dm.read_inode(id)
                    && ino.mode == FileType::Directory
                {
                    w.write_u32(NFS3ERR_ISDIR);
                    self.write_wcc_data(&mut w, dir_id, pre_dir.as_ref(), pre_dir.as_ref());
                    return w.into_bytes();
                }
                id
            }
            Err(e) => {
                w.write_u32(disk_error_to_nfs3(&e));
                self.write_wcc_data(&mut w, dir_id, pre_dir.as_ref(), pre_dir.as_ref());
                return w.into_bytes();
            }
        };

        let status = match self.dm.delete_file(dir_id, &name) {
            Ok(()) => {
                self.forget_parent(child_id);
                NFS3_OK
            }
            Err(e) => disk_error_to_nfs3(&e),
        };
        let post_dir = self.read_inode_opt(dir_id);
        w.write_u32(status);
        self.write_wcc_data(&mut w, dir_id, pre_dir.as_ref(), post_dir.as_ref());
        w.into_bytes()
    }

    fn nfs_rmdir(&self, xid: u32, r: &mut XdrReader<'_>) -> Vec<u8> {
        let mut w = begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS);
        let dir_id = match self.decode_handle(r) {
            Ok(id) => id,
            Err(status) => {
                w.write_u32(status);
                self.write_wcc_data(&mut w, 0, None, None);
                return w.into_bytes();
            }
        };
        let Some(raw_name) = r.read_string(4096) else {
            return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes();
        };

        let pre_dir = self.read_inode_opt(dir_id);
        if self.is_read_only() {
            w.write_u32(NFS3ERR_ROFS);
            self.write_wcc_data(&mut w, dir_id, pre_dir.as_ref(), pre_dir.as_ref());
            return w.into_bytes();
        }

        let name = normalize_filename_nfc(raw_name);
        let child_id = match self.dm.lookup(dir_id, &name) {
            Ok(id) => id,
            Err(e) => {
                w.write_u32(disk_error_to_nfs3(&e));
                self.write_wcc_data(&mut w, dir_id, pre_dir.as_ref(), pre_dir.as_ref());
                return w.into_bytes();
            }
        };

        if let Ok(ino) = self.dm.read_inode(child_id)
            && ino.mode != FileType::Directory
        {
            w.write_u32(NFS3ERR_NOTDIR);
            self.write_wcc_data(&mut w, dir_id, pre_dir.as_ref(), pre_dir.as_ref());
            return w.into_bytes();
        }
        if let Ok(entries) = self.dm.list_dir(child_id)
            && !entries.is_empty()
        {
            w.write_u32(NFS3ERR_NOTEMPTY);
            self.write_wcc_data(&mut w, dir_id, pre_dir.as_ref(), pre_dir.as_ref());
            return w.into_bytes();
        }

        let status = match self.dm.delete_file(dir_id, &name) {
            Ok(()) => {
                self.forget_parent(child_id);
                NFS3_OK
            }
            Err(e) => disk_error_to_nfs3(&e),
        };
        let post_dir = self.read_inode_opt(dir_id);
        w.write_u32(status);
        self.write_wcc_data(&mut w, dir_id, pre_dir.as_ref(), post_dir.as_ref());
        w.into_bytes()
    }

    fn nfs_rename(&self, xid: u32, r: &mut XdrReader<'_>) -> Vec<u8> {
        let mut w = begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS);
        let from_dir = match self.decode_handle(r) {
            Ok(id) => id,
            Err(status) => {
                w.write_u32(status);
                self.write_wcc_data(&mut w, 0, None, None);
                self.write_wcc_data(&mut w, 0, None, None);
                return w.into_bytes();
            }
        };
        let Some(raw_from) = r.read_string(4096) else {
            return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes();
        };
        let to_dir = match self.decode_handle(r) {
            Ok(id) => id,
            Err(status) => {
                w.write_u32(status);
                let pre_from = self.read_inode_opt(from_dir);
                self.write_wcc_data(&mut w, from_dir, pre_from.as_ref(), pre_from.as_ref());
                self.write_wcc_data(&mut w, 0, None, None);
                return w.into_bytes();
            }
        };
        let Some(raw_to) = r.read_string(4096) else {
            return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes();
        };

        let pre_from = self.read_inode_opt(from_dir);
        let pre_to = self.read_inode_opt(to_dir);
        if self.is_read_only() {
            w.write_u32(NFS3ERR_ROFS);
            self.write_wcc_data(&mut w, from_dir, pre_from.as_ref(), pre_from.as_ref());
            self.write_wcc_data(&mut w, to_dir, pre_to.as_ref(), pre_to.as_ref());
            return w.into_bytes();
        }

        let from_name = normalize_filename_nfc(raw_from);
        let to_name = normalize_filename_nfc(raw_to);
        let moved_id = self.dm.lookup(from_dir, &from_name).ok();
        let overwritten_id = self.dm.lookup(to_dir, &to_name).ok();

        let status = match self.dm.rename(from_dir, &from_name, to_dir, &to_name) {
            Ok(()) => {
                if let (Some(dst_id), Some(src_id)) = (overwritten_id, moved_id)
                    && dst_id != src_id
                {
                    self.forget_parent(dst_id);
                }
                if let Some(id) = moved_id
                    && let Ok(ino) = self.dm.read_inode(id)
                    && ino.mode == FileType::Directory
                {
                    self.record_parent(id, to_dir);
                }
                NFS3_OK
            }
            Err(e) => disk_error_to_nfs3(&e),
        };

        let post_from = self.read_inode_opt(from_dir);
        let post_to = self.read_inode_opt(to_dir);
        w.write_u32(status);
        self.write_wcc_data(&mut w, from_dir, pre_from.as_ref(), post_from.as_ref());
        self.write_wcc_data(&mut w, to_dir, pre_to.as_ref(), post_to.as_ref());
        w.into_bytes()
    }

    fn sorted_dir_entries(
        &self,
        sess: &crate::disk::ReadSession<'_>,
        dir_id: u64,
        dir_ino: Option<&Inode>,
    ) -> Result<NfsSortedDirEntries, u32> {
        let key_opt = dir_ino.map(|ino| NfsDirSnapshotKey {
            dir_id,
            write_gen: sess.inode_gen(dir_id),
            size: ino.size,
            modified_at: ino.modified_at,
        });
        if let Some(key) = key_opt {
            let guard = crate::read_unpoisoned(&self.dir_snapshots);
            if let Some((_, entries)) = guard.iter().rev().find(|(k, _)| *k == key) {
                return Ok(Arc::clone(entries));
            }
        }
        let mut entries = sess.list_dir(dir_id).map_err(|e| disk_error_to_nfs3(&e))?;
        entries.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.inode.cmp(&b.inode)));
        let snapshot: NfsSortedDirEntries = Arc::from(
            entries
                .into_iter()
                .map(|e| (e.name, e.inode))
                .collect::<Vec<_>>(),
        );
        if let Some(key) = key_opt {
            let mut guard = crate::write_unpoisoned(&self.dir_snapshots);
            if let Some((_, existing)) = guard.iter().rev().find(|(k, _)| *k == key) {
                return Ok(Arc::clone(existing));
            }
            guard.retain(|(k, _)| k.dir_id != dir_id);
            if guard.len() >= NFS_DIR_SNAPSHOT_CAP {
                guard.pop_front();
            }
            guard.push_back((key, Arc::clone(&snapshot)));
        }
        Ok(snapshot)
    }

    fn nfs_readdir(&self, xid: u32, r: &mut XdrReader<'_>) -> Vec<u8> {
        let mut w = begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS);
        let dir_id = match self.decode_handle(r) {
            Ok(id) => id,
            Err(status) => {
                w.write_u32(status);
                w.write_bool(false);
                return w.into_bytes();
            }
        };
        let Some(cookie) = r.read_u64() else {
            return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes();
        };
        if r.read_fixed_opaque(8).is_none() {
            return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes();
        }
        let Some(count) = r.read_u32() else {
            return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes();
        };

        let sess = self.dm.begin_read();
        let dir_ino = sess.read_allocated_inode(dir_id).ok();
        let entries = match self.sorted_dir_entries(&sess, dir_id, dir_ino.as_ref()) {
            Ok(list) => list,
            Err(status) => {
                w.write_u32(status);
                self.write_post_op_attr(&mut w, dir_id, dir_ino.as_ref());
                return w.into_bytes();
            }
        };

        let max_bytes = (count as usize).clamp(256, NFS_MAX_IO_SIZE as usize);
        let start_len = w.len();
        w.write_u32(NFS3_OK);
        self.write_post_op_attr(&mut w, dir_id, dir_ino.as_ref());
        w.write_fixed_opaque(&self.write_verf); // cookieverf3

        let parent_id = self.parent_of(dir_id);
        let mut eof = true;
        let mut emitted = 0usize;

        let special: [(u64, &str, u64); 2] = [(1, ".", dir_id), (2, "..", parent_id)];
        for (entry_cookie, name, ino_id) in special {
            if entry_cookie <= cookie {
                continue;
            }
            let name_padded = xdr_padded_len(name.len()).unwrap_or(name.len());
            let entry_bytes = 4 + 8 + 4 + name_padded + 8;
            let used = w.len() - start_len;
            if emitted > 0 && used + entry_bytes + 8 > max_bytes {
                eof = false;
                w.write_bool(false);
                w.write_bool(eof);
                return w.into_bytes();
            }
            w.write_bool(true);
            w.write_u64(ino_id.saturating_add(1));
            w.write_string(name);
            w.write_u64(entry_cookie);
            emitted += 1;
        }

        let start_idx = usize::try_from(cookie.saturating_sub(2)).unwrap_or(usize::MAX);
        for (idx, (name, ino_id)) in entries.iter().enumerate().skip(start_idx) {
            let entry_cookie = (idx as u64) + 3;
            let name_padded = xdr_padded_len(name.len()).unwrap_or(name.len());
            let entry_bytes = 4 + 8 + 4 + name_padded + 8;
            let used = w.len() - start_len;
            if emitted > 0 && used + entry_bytes + 8 > max_bytes {
                eof = false;
                break;
            }
            w.write_bool(true);
            w.write_u64(ino_id.saturating_add(1));
            w.write_string(name);
            w.write_u64(entry_cookie);
            emitted += 1;
        }

        w.write_bool(false); // end of entry3 list
        w.write_bool(eof);
        w.into_bytes()
    }

    fn nfs_readdirplus(&self, xid: u32, r: &mut XdrReader<'_>) -> Vec<u8> {
        let mut w = begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS);
        let dir_id = match self.decode_handle(r) {
            Ok(id) => id,
            Err(status) => {
                w.write_u32(status);
                w.write_bool(false);
                return w.into_bytes();
            }
        };
        let Some(cookie) = r.read_u64() else {
            return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes();
        };
        if r.read_fixed_opaque(8).is_none() {
            return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes();
        }
        let (Some(dircount), Some(maxcount)) = (r.read_u32(), r.read_u32()) else {
            return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes();
        };

        let sess = self.dm.begin_read();
        let dir_ino = sess.read_allocated_inode(dir_id).ok();
        let entries = match self.sorted_dir_entries(&sess, dir_id, dir_ino.as_ref()) {
            Ok(list) => list,
            Err(status) => {
                w.write_u32(status);
                self.write_post_op_attr(&mut w, dir_id, dir_ino.as_ref());
                return w.into_bytes();
            }
        };

        let max_total = (maxcount as usize).clamp(512, NFS_MAX_IO_SIZE as usize);
        let max_dir = (dircount as usize).clamp(256, max_total);
        let start_len = w.len();

        w.write_u32(NFS3_OK);
        self.write_post_op_attr(&mut w, dir_id, dir_ino.as_ref());
        w.write_fixed_opaque(&self.write_verf); // cookieverf3

        let parent_id = self.parent_of(dir_id);
        let mut eof = true;
        let mut emitted = 0usize;
        let mut dir_bytes_used = 0usize;

        let special: [(u64, &str, u64); 2] = [(1, ".", dir_id), (2, "..", parent_id)];
        for (entry_cookie, name, ino_id) in special {
            if entry_cookie <= cookie {
                continue;
            }
            let name_padded = xdr_padded_len(name.len()).unwrap_or(name.len());
            let base_dir_bytes = 8 + 4 + name_padded + 8;
            let full_entry_bytes = 136 + name_padded;
            let total_used = w.len() - start_len;
            if emitted > 0
                && (total_used + full_entry_bytes + 8 > max_total
                    || dir_bytes_used + base_dir_bytes > max_dir)
            {
                eof = false;
                w.write_bool(false);
                w.write_bool(eof);
                return w.into_bytes();
            }
            let child_ino = if ino_id == dir_id {
                dir_ino
            } else {
                sess.read_allocated_inode(ino_id).ok()
            };
            w.write_bool(true);
            w.write_u64(ino_id.saturating_add(1));
            w.write_string(name);
            w.write_u64(entry_cookie);
            self.write_post_op_attr(&mut w, ino_id, child_ino.as_ref());
            self.write_post_op_fh(&mut w, Some(ino_id));

            emitted += 1;
            dir_bytes_used += base_dir_bytes;
        }

        let start_idx = usize::try_from(cookie.saturating_sub(2)).unwrap_or(usize::MAX);
        for (idx, (name, ino_id)) in entries.iter().enumerate().skip(start_idx) {
            let entry_cookie = (idx as u64) + 3;
            let name_padded = xdr_padded_len(name.len()).unwrap_or(name.len());
            let base_dir_bytes = 8 + 4 + name_padded + 8;
            let full_entry_bytes = 136 + name_padded;
            let total_used = w.len() - start_len;

            if emitted > 0
                && (total_used + full_entry_bytes + 8 > max_total
                    || dir_bytes_used + base_dir_bytes > max_dir)
            {
                eof = false;
                break;
            }

            let child_ino = sess.read_allocated_inode(*ino_id).ok();
            if let Some(ref ino) = child_ino
                && ino.mode == FileType::Directory
            {
                self.record_parent(*ino_id, dir_id);
            }
            w.write_bool(true);
            w.write_u64(ino_id.saturating_add(1));
            w.write_string(name);
            w.write_u64(entry_cookie);
            self.write_post_op_attr(&mut w, *ino_id, child_ino.as_ref());
            self.write_post_op_fh(&mut w, Some(*ino_id));

            emitted += 1;
            dir_bytes_used += base_dir_bytes;
        }

        w.write_bool(false); // end of entryplus3 list
        w.write_bool(eof);
        w.into_bytes()
    }

    fn nfs_fsstat(&self, xid: u32, r: &mut XdrReader<'_>) -> Vec<u8> {
        let mut w = begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS);
        let inode_id = match self.decode_handle(r) {
            Ok(id) => id,
            Err(status) => {
                w.write_u32(status);
                w.write_bool(false);
                return w.into_bytes();
            }
        };

        let ino = self.read_inode_opt(inode_id);
        let sb = self.dm.superblock();
        let frag = match self.dm.analyze_fragmentation() {
            Ok(f) => f,
            Err(e) => {
                w.write_u32(disk_error_to_nfs3(&e));
                self.write_post_op_attr(&mut w, inode_id, ino.as_ref());
                return w.into_bytes();
            }
        };

        let block_size = u64::from(sb.block_size.max(BLOCK_SIZE as u32));
        let tbytes = (frag.total_blocks as u64).saturating_mul(block_size);
        let fbytes = (frag.free_blocks as u64).saturating_mul(block_size);
        let abytes = if self.is_read_only() { 0 } else { fbytes };
        let tfiles = sb.inode_count;
        // Count allocated inodes conservatively (at least root = 1)
        let ffiles = sb.inode_count.saturating_sub(1);
        let afiles = if self.is_read_only() { 0 } else { ffiles };

        w.write_u32(NFS3_OK);
        self.write_post_op_attr(&mut w, inode_id, ino.as_ref());
        w.write_u64(tbytes);
        w.write_u64(fbytes);
        w.write_u64(abytes);
        w.write_u64(tfiles);
        w.write_u64(ffiles);
        w.write_u64(afiles);
        w.write_u32(0); // invarsec
        w.into_bytes()
    }

    fn nfs_fsinfo(&self, xid: u32, r: &mut XdrReader<'_>) -> Vec<u8> {
        let mut w = begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS);
        let inode_id = match self.decode_handle(r) {
            Ok(id) => id,
            Err(status) => {
                w.write_u32(status);
                w.write_bool(false);
                return w.into_bytes();
            }
        };

        let ino = self.read_inode_opt(inode_id);
        w.write_u32(NFS3_OK);
        self.write_post_op_attr(&mut w, inode_id, ino.as_ref());
        w.write_u32(NFS_MAX_IO_SIZE); // rtmax
        w.write_u32(NFS_PREFERRED_IO_SIZE); // rtpref
        w.write_u32(BLOCK_SIZE as u32); // rtmult
        w.write_u32(NFS_MAX_IO_SIZE); // wtmax
        w.write_u32(NFS_PREFERRED_IO_SIZE); // wtpref
        w.write_u32(BLOCK_SIZE as u32); // wtmult
        w.write_u32(NFS_PREFERRED_READDIR_SIZE); // dtpref
        w.write_u64(512u64 * 1024 * 1024 * 1024); // maxfilesize (512 GiB)
        // time_delta: 1 second
        w.write_u32(1);
        w.write_u32(0);
        w.write_u32(FSF3_HOMOGENEOUS | FSF3_CANSETTIME);
        w.into_bytes()
    }

    fn nfs_pathconf(&self, xid: u32, r: &mut XdrReader<'_>) -> Vec<u8> {
        let mut w = begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS);
        let inode_id = match self.decode_handle(r) {
            Ok(id) => id,
            Err(status) => {
                w.write_u32(status);
                w.write_bool(false);
                return w.into_bytes();
            }
        };

        let ino = self.read_inode_opt(inode_id);
        w.write_u32(NFS3_OK);
        self.write_post_op_attr(&mut w, inode_id, ino.as_ref());
        w.write_u32(1); // linkmax
        w.write_u32(DirectoryEntry::MAX_FILENAME_LEN as u32); // name_max
        w.write_bool(true); // no_trunc
        w.write_bool(true); // chown_restricted
        w.write_bool(false); // case_insensitive
        w.write_bool(true); // case_preserving
        w.into_bytes()
    }

    fn nfs_commit(&self, xid: u32, r: &mut XdrReader<'_>) -> Vec<u8> {
        let mut w = begin_accepted_reply(xid, RPC_ACCEPT_SUCCESS);
        let inode_id = match self.decode_handle(r) {
            Ok(id) => id,
            Err(status) => {
                w.write_u32(status);
                self.write_wcc_data(&mut w, 0, None, None);
                return w.into_bytes();
            }
        };
        if r.read_u64().is_none() || r.read_u32().is_none() {
            return begin_accepted_reply(xid, RPC_ACCEPT_GARBAGE_ARGS).into_bytes();
        }

        let pre = self.read_inode_opt(inode_id);
        if !self.is_read_only()
            && let Err(e) = self.dm.flush()
        {
            let post = self.read_inode_opt(inode_id);
            w.write_u32(disk_error_to_nfs3(&e));
            self.write_wcc_data(&mut w, inode_id, pre.as_ref(), post.as_ref());
            return w.into_bytes();
        }
        let post = self.read_inode_opt(inode_id);
        w.write_u32(NFS3_OK);
        self.write_wcc_data(&mut w, inode_id, pre.as_ref(), post.as_ref());
        w.write_fixed_opaque(&self.write_verf);
        w.into_bytes()
    }
}

// ---------------------------------------------------------------------------
// Multi-Connection Loopback TCP Server & Native OS Mount Helpers
// ---------------------------------------------------------------------------

#[inline]
fn poll_fd_readable(fd: RawFd, timeout_ms: i32) -> io::Result<bool> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: `pfd` is a valid single-element pollfd struct.
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if rc > 0 {
            return Ok((pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)) != 0);
        }
        if rc == 0 {
            return Ok(false);
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }
}

#[cfg(target_os = "linux")]
fn verify_linux_loopback_peer_uid(stream: &TcpStream, owner_uid: u32) -> bool {
    let (Ok(peer), Ok(local)) = (stream.peer_addr(), stream.local_addr()) else {
        return true;
    };
    let (std::net::SocketAddr::V4(peer_v4), std::net::SocketAddr::V4(local_v4)) = (peer, local)
    else {
        return true;
    };
    // Loopback 127.0.0.1 in /proc/net/tcp is little-endian 0100007F.
    let target_local = format!("0100007F:{:04X}", local_v4.port());
    let target_rem = format!("0100007F:{:04X}", peer_v4.port());
    let Ok(proc_tcp) = std::fs::read_to_string("/proc/net/tcp") else {
        return true;
    };
    for line in proc_tcp.lines().skip(1) {
        let mut cols = line.split_whitespace();
        let _sl = cols.next();
        let Some(local_hex) = cols.next() else {
            continue;
        };
        let Some(rem_hex) = cols.next() else { continue };
        let Some(st) = cols.next() else { continue };
        // Established ("01") client side has local_address == client ephemeral (rem_hex) and rem_address == server (target_local).
        if st == "01" && local_hex == target_rem && rem_hex == target_local {
            // columns after rem_address: st(3), tx_rx_q(4), tr_tm(5), retrnsmt(6), uid(7)
            if let Some(uid_str) = cols.nth(3)
                && let Ok(sock_uid) = uid_str.parse::<u32>()
            {
                return sock_uid == owner_uid || sock_uid == 0;
            }
        }
    }
    true
}

fn serve_tcp_connection(mut stream: TcpStream, fs: Arc<OifsNfsFs>, shutdown: Arc<AtomicBool>) {
    let _ = stream.set_nodelay(true);
    let raw_fd = stream.as_raw_fd();

    loop {
        match poll_fd_readable(raw_fd, 50) {
            Ok(false) => {
                if shutdown.load(Ordering::Acquire) {
                    break;
                }
                continue;
            }
            Ok(true) => {}
            Err(_) => break,
        }

        let record = match read_rpc_record(&mut stream) {
            Ok(Some(rec)) => rec,
            Ok(None) | Err(_) => break,
        };

        let reply = fs.dispatch_rpc(&record);
        if write_rpc_record(&mut stream, &reply).is_err() {
            break;
        }
    }

    let _ = stream.shutdown(Shutdown::Both);
}

/// Embedded loopback TCP NFSv3 + MOUNT v3 server bound strictly to `127.0.0.1:<port>`.
pub struct OifsNfsServer {
    port: u16,
    fs: Arc<OifsNfsFs>,
    shutdown: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
}

impl OifsNfsServer {
    /// Binds a loopback TCP listener on `127.0.0.1:<options.port>` and starts serving
    /// MOUNT v3 + NFS v3 connections in the background.
    pub fn bind(dm: Arc<DiskManager>, options: NfsMountOptions) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", options.port))?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;

        let fs = Arc::new(OifsNfsFs::from_arc(dm, options));
        let shutdown = Arc::new(AtomicBool::new(false));

        let fs_bg = Arc::clone(&fs);
        let shutdown_bg = Arc::clone(&shutdown);
        let active_conns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let accept_thread = thread::Builder::new()
            .name(format!("oifs-nfs-{port}"))
            .spawn(move || {
                struct ConnCountGuard(Arc<std::sync::atomic::AtomicUsize>);
                impl Drop for ConnCountGuard {
                    fn drop(&mut self) {
                        self.0.fetch_sub(1, Ordering::AcqRel);
                    }
                }

                let raw_fd = listener.as_raw_fd();
                let mut workers: Vec<JoinHandle<()>> = Vec::new();

                while !shutdown_bg.load(Ordering::Acquire) {
                    workers.retain(|h| !h.is_finished());
                    match poll_fd_readable(raw_fd, 50) {
                        Ok(true) => loop {
                            match listener.accept() {
                                Ok((stream, _)) => {
                                    // Atomically reserve a connection slot up to MAX_CONCURRENT_NFS_CONNS.
                                    let mut reserved = false;
                                    let mut cur = active_conns.load(Ordering::Acquire);
                                    while cur < MAX_CONCURRENT_NFS_CONNS {
                                        match active_conns.compare_exchange_weak(
                                            cur,
                                            cur + 1,
                                            Ordering::AcqRel,
                                            Ordering::Acquire,
                                        ) {
                                            Ok(_) => {
                                                reserved = true;
                                                break;
                                            }
                                            Err(actual) => cur = actual,
                                        }
                                    }
                                    if !reserved {
                                        drop(stream);
                                        continue;
                                    }
                                    let _ = stream.set_nonblocking(false);
                                    let _ = stream.set_read_timeout(Some(NFS_CONN_IO_TIMEOUT));
                                    let _ = stream.set_write_timeout(Some(NFS_CONN_IO_TIMEOUT));
                                    let fs_w = Arc::clone(&fs_bg);
                                    #[cfg(target_os = "linux")]
                                    if !fs_w.options.allow_other
                                        && !verify_linux_loopback_peer_uid(
                                            &stream,
                                            fs_w.options.uid,
                                        )
                                    {
                                        drop(stream);
                                        continue;
                                    }
                                    let sh_w = Arc::clone(&shutdown_bg);
                                    let conn_guard = ConnCountGuard(Arc::clone(&active_conns));
                                    if let Ok(h) = thread::Builder::new()
                                        .name("oifs-nfs-conn".into())
                                        .spawn(move || {
                                            let _guard = conn_guard;
                                            serve_tcp_connection(stream, fs_w, sh_w);
                                        })
                                    {
                                        workers.push(h);
                                    }
                                }
                                Err(e)
                                    if e.kind() == io::ErrorKind::WouldBlock
                                        || e.kind() == io::ErrorKind::Interrupted =>
                                {
                                    break;
                                }
                                Err(_) => break,
                            }
                        },
                        Ok(false) => {}
                        Err(_) => break,
                    }
                }

                for h in workers {
                    let _ = h.join();
                }
            })?;

        Ok(Self {
            port,
            fs,
            shutdown,
            accept_thread: Some(accept_thread),
        })
    }

    /// Returns the bound loopback TCP port.
    #[inline]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Returns the underlying protocol dispatcher.
    #[inline]
    pub fn fs(&self) -> &Arc<OifsNfsFs> {
        &self.fs
    }

    /// Returns a handle to the server's shutdown flag (for signal-driven unmount).
    #[inline]
    pub fn shutdown_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shutdown)
    }

    /// Signals the accept loop and all active connection workers to stop and joins them.
    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(h) = self.accept_thread.take() {
            let _ = h.join();
        }
    }
}

impl Drop for OifsNfsServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Mounts a running local OIFS NFSv3 server onto `mountpoint` using the host OS's built-in
/// NFS client (`mount_nfs` on macOS, `mount -t nfs` on Linux).
pub fn mount_os_nfs<P: AsRef<Path>>(port: u16, mountpoint: P, read_only: bool) -> io::Result<()> {
    let mp = mountpoint.as_ref();
    if !mp.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("mountpoint {:?} does not exist", mp),
        ));
    }

    #[cfg(target_os = "macos")]
    {
        let mut opts = format!(
            "nolocks,vers=3,tcp,rsize={NFS_PREFERRED_IO_SIZE},wsize={NFS_PREFERRED_IO_SIZE},port={port},mountport={port}"
        );
        if read_only {
            opts.push_str(",rdonly");
        }
        let output = Command::new("mount_nfs")
            .args(["-o", &opts, "127.0.0.1:/"])
            .arg(mp)
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(io::Error::other(format!(
                "mount_nfs failed (status {}): {}",
                output.status,
                stderr.trim()
            )));
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    {
        let mut opts = format!(
            "nolock,vers=3,proto=tcp,rsize={NFS_PREFERRED_IO_SIZE},wsize={NFS_PREFERRED_IO_SIZE},port={port},mountport={port}"
        );
        if read_only {
            opts.push_str(",ro");
        }
        let output = Command::new("mount")
            .args(["-t", "nfs", "-o", &opts, "127.0.0.1:/"])
            .arg(mp)
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(io::Error::other(format!(
                "mount -t nfs failed (status {}): {}",
                output.status,
                stderr.trim()
            )));
        }
        Ok(())
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (port, mp, read_only);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "OS NFS mount is only supported on macOS and Linux",
        ))
    }
}

/// Checks whether `mountpoint` is currently a different filesystem device than its parent
/// directory (i.e. an active mountpoint).
pub fn is_mounted<P: AsRef<Path>>(mountpoint: P) -> bool {
    use std::os::unix::fs::MetadataExt;
    let mp = mountpoint.as_ref();
    let Ok(mp_meta) = std::fs::metadata(mp) else {
        return false;
    };
    let Some(parent) = mp.parent() else {
        return true;
    };
    let parent_path = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let Ok(parent_meta) = std::fs::metadata(parent_path) else {
        return false;
    };
    mp_meta.dev() != parent_meta.dev()
}

/// Starts a loopback NFSv3 server, mounts it onto `mountpoint`, and blocks until the
/// filesystem is unmounted externally or `shutdown` is set to `true`.
pub fn mount_and_serve_nfs<P: AsRef<Path>>(
    dm: Arc<DiskManager>,
    mountpoint: P,
    mut options: NfsMountOptions,
    external_shutdown: Option<Arc<AtomicBool>>,
) -> io::Result<()> {
    let mp = mountpoint.as_ref();
    let read_only = options.read_only || dm.is_read_only();
    options.require_auth_sys = true;
    let mut server = OifsNfsServer::bind(Arc::clone(&dm), options)?;
    if let Err(e) = mount_os_nfs(server.port(), mp, read_only) {
        server.stop();
        return Err(e);
    }
    server.fs().seal_mount();

    // Block until external unmount or shutdown signal.
    loop {
        if external_shutdown
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            let _ = crate::fuse::unmount(mp);
            break;
        }
        if !is_mounted(mp) {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }

    server.stop();
    if !read_only {
        let _ = dm.flush();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Formal Verification (Kani Proofs)
// ---------------------------------------------------------------------------

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    /// Proves that `xdr_padded_len` aligns to 4 bytes without overflow and `XdrReader`
    /// bounds checks never panic for arbitrary slices and declared lengths.
    #[kani::proof]
    fn kani_xdr_padding_and_bounds() {
        let len: usize = kani::any();
        match xdr_padded_len(len) {
            Some(padded) => {
                assert!(padded >= len);
                assert!(padded - len < 4);
                assert_eq!(padded & 3, 0);
            }
            None => {
                assert!(len > usize::MAX - 3);
            }
        }

        let buf: [u8; 16] = kani::any();
        let mut r = XdrReader::new(&buf);
        let max_len: usize = kani::any();
        kani::assume(max_len <= 64);
        if let Some(slice) = r.read_opaque(max_len) {
            assert!(slice.len() <= max_len);
            assert!(r.position() <= buf.len());
            assert_eq!(r.position() & 3, 0);
        }
    }

    /// Proves that ONC RPC TCP record-marking header parsing and `RpcCallHeader::parse`
    /// are completely panic-free for arbitrary input bytes.
    #[kani::proof]
    fn kani_rpc_record_marking_framing() {
        let hdr: [u8; 4] = kani::any();
        let (is_last, frag_len) = parse_record_marking_header(hdr);
        assert!(frag_len <= 0x7FFF_FFFF);
        assert_eq!(is_last, (hdr[0] & 0x80) != 0);

        let raw: [u8; 48] = kani::any();
        let slice_len: usize = kani::any();
        kani::assume(slice_len <= raw.len());
        let mut r = XdrReader::new(&raw[..slice_len]);
        if let Some(call) = RpcCallHeader::parse(&mut r) {
            assert!(r.position() <= slice_len);
            assert_eq!(r.position() & 3, 0);
            let _ = (call.xid, call.prog, call.vers, call.proc_num);
        }
    }

    /// Proves that `NfsFileHandle` serialization `(fs_generation, inode_id) <-> [u8; 16]`
    /// is a lossless bijection and rejects non-16-byte slices.
    #[kani::proof]
    fn kani_nfs_filehandle_roundtrip() {
        let fs_gen: u64 = kani::any();
        let ino: u64 = kani::any();
        let fh = NfsFileHandle::new(fs_gen, ino);
        let bytes = fh.to_bytes();
        let decoded = NfsFileHandle::from_bytes(&bytes).expect("16-byte handle must decode");
        assert_eq!(decoded, fh);

        let short: [u8; 15] = kani::any();
        assert!(NfsFileHandle::from_bytes(&short).is_none());
    }

    /// Proves that `parse_auth_sys_uid_gid` is panic-free for arbitrary truncated/malformed
    /// inputs and `is_nfs_uid_authorized` enforces owner-only UID gating unless `allow_other`.
    #[kani::proof]
    fn kani_auth_sys_parsing_and_uid_gate() {
        let raw: [u8; 48] = kani::any();
        let slice_len: usize = kani::any();
        kani::assume(slice_len <= raw.len());
        let _ = parse_auth_sys_uid_gid(&raw[..slice_len]);

        let caller_uid: u32 = kani::any();
        let owner_uid: u32 = kani::any();
        let allow_other: bool = kani::any();
        let ok = is_nfs_uid_authorized(caller_uid, owner_uid, allow_other);
        assert_eq!(
            ok,
            allow_other || caller_uid == owner_uid || caller_uid == 0
        );
    }
}
