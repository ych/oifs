//! IPC communication module for OIFS file system
//!
//! Provides dual-mode inter-process communication:
//! 1. Default Local Mode: Unix Domain Socket (UDS) based in `/tmp`
//! 2. Optional Network Mode: Rendezvous File (`.image.master`) in the image directory
//!    + TCP Transport + Clock-Free Active Ping Probe for NFS/Lustre/multi-node clusters.

use std::cell::RefCell;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use bincode::Options as _;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::directory::DirectoryEntry;
use crate::disk::{
    CompressionMode, DefragMode, DefragStats, DiskManager, DiskManagerError, FragmentationStats,
    FsckReport, MigrationStats,
};
use crate::inode::Inode;
use crate::superblock::SuperBlock;

/// Session transport mode
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SessionMode {
    /// Default: Standalone mode (IPC disabled, direct DiskManager access, no sockets or server threads)
    #[default]
    Standalone,
    /// Local machine IPC mode, using Unix Domain Socket (UDS) in `/tmp`
    Local,
    /// Network / Cluster mode: uses Rendezvous master file in the image directory + TCP transport + Active Ping Probe
    Network {
        /// Custom bind address (e.g. "0.0.0.0:9050" or "127.0.0.1:0")
        bind_addr: Option<String>,
    },
}

impl SessionMode {
    /// Returns `true` if IPC is enabled (`Local` or `Network`).
    pub fn is_ipc_enabled(&self) -> bool {
        !matches!(self, SessionMode::Standalone)
    }
}

/// Metadata stored in the `.image.master` rendezvous file for Network Mode
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MasterInfo {
    pub addr: String,
    pub pid: u32,
    /// Hex-encoded session secret used to authenticate IPC clients.
    ///
    /// The rendezvous file is written with owner-only permissions, so only
    /// processes that may read the image directory can obtain it.
    #[serde(default)]
    pub auth_token: String,
}

/// Abstract Stream supporting both Unix Domain Sockets and TCP
pub enum IpcStream {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Read for IpcStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            IpcStream::Unix(s) => s.read(buf),
            IpcStream::Tcp(s) => s.read(buf),
        }
    }
}

impl Write for IpcStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            IpcStream::Unix(s) => s.write(buf),
            IpcStream::Tcp(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            IpcStream::Unix(s) => s.flush(),
            IpcStream::Tcp(s) => s.flush(),
        }
    }
}

impl IpcStream {
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match self {
            IpcStream::Unix(s) => s.set_nonblocking(nonblocking),
            IpcStream::Tcp(s) => s.set_nonblocking(nonblocking),
        }
    }

    /// Sets the read timeout of the underlying socket.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            IpcStream::Unix(s) => s.set_read_timeout(timeout),
            IpcStream::Tcp(s) => s.set_read_timeout(timeout),
        }
    }

    /// Sets the write timeout of the underlying socket.
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            IpcStream::Unix(s) => s.set_write_timeout(timeout),
            IpcStream::Tcp(s) => s.set_write_timeout(timeout),
        }
    }

    /// Shuts the connection down in both directions.
    pub fn shutdown(&self, how: std::net::Shutdown) -> io::Result<()> {
        match self {
            IpcStream::Unix(s) => s.shutdown(how),
            IpcStream::Tcp(s) => s.shutdown(how),
        }
    }
}

/// Abstract Listener supporting both Unix Domain Sockets and TCP
pub enum IpcListener {
    Unix {
        listener: UnixListener,
        socket_path: PathBuf,
        /// File holding the session secret, readable by the owner only.
        token_path: PathBuf,
        /// The session secret published to legitimate peers.
        auth_secret: [u8; AUTH_SECRET_LEN],
    },
    Tcp {
        listener: TcpListener,
        master_file_path: PathBuf,
        /// The session secret published to legitimate peers.
        auth_secret: [u8; AUTH_SECRET_LEN],
    },
}

/// Event types emitted when other processes interact with the master
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionEvent {
    /// A new peer process has connected
    PeerConnected { peer_id: usize },
    /// A peer process has disconnected
    PeerDisconnected { peer_id: usize },
    /// A request was handled by the master on behalf of a peer
    RequestHandled { peer_id: usize, req_type: String },
}

/// Computes a unique, deterministic Unix Domain Socket path for a given image file.
///
/// The socket lives in a per-user private directory (mode `0700`) instead of
/// the world-writable `/tmp`, and the socket itself is created with mode
/// `0600`. Both restrict access to the owner's own processes (SEC-18).
pub fn get_socket_path<P: AsRef<Path>>(image_path: P) -> PathBuf {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let abs_path = crate::session::canonicalize_path(image_path);

    let mut hasher = DefaultHasher::new();
    abs_path.to_string_lossy().hash(&mut hasher);
    let hash = hasher.finish();

    let file_name = abs_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "image".to_string());

    let safe_name: String = file_name
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .take(12)
        .collect();

    get_private_runtime_dir().join(format!("oifs_{}_{:016x}.sock", safe_name, hash))
}

/// Path of the file that holds the session secret of a local (UDS) master.
pub fn get_token_path<P: AsRef<Path>>(image_path: P) -> PathBuf {
    let mut path = get_socket_path(image_path).into_os_string();
    path.push(".tok");
    PathBuf::from(path)
}

/// Returns the per-user private directory used for IPC sockets and tokens.
///
/// The directory is created with mode `0700`, so only the owning user can
/// reach the socket or read the session secret. `$XDG_RUNTIME_DIR` is used
/// when it is set, otherwise a `oifs-<uid>` directory below the temp dir.
pub fn get_private_runtime_dir() -> PathBuf {
    let base = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => std::env::temp_dir().join(format!("oifs-{}", unsafe { libc::getuid() })),
    };
    let dir = base.join("oifs");
    if !dir.exists() {
        let _ = std::fs::create_dir_all(&dir);
        restrict_permissions(&dir, 0o700);
    }
    dir
}

/// Restricts a path to the given POSIX mode (best effort).
fn restrict_permissions(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

/// Computes the path for the `.image.master` rendezvous file in the image directory
pub fn get_master_info_path<P: AsRef<Path>>(image_path: P) -> PathBuf {
    let abs_p = crate::session::canonicalize_path(image_path);

    let file_name = abs_p
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "disk.img".to_string());
    let parent = abs_p.parent().unwrap_or(Path::new("."));
    parent.join(format!(".{}.master", file_name))
}

/// Probes a TCP master address to verify if it is alive (Clock-Free Active Ping Probe)
pub fn is_tcp_master_alive(addr_str: &str) -> bool {
    let addrs: Vec<_> = match addr_str.to_socket_addrs() {
        Ok(iter) => iter.collect(),
        Err(_) => return false,
    };

    for addr in addrs {
        if let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
            let _ = stream.set_nodelay(true);
            let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
            let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
            if write_framed(&mut stream, &IpcRequest::Ping).is_ok()
                && let Ok(IpcResponse::Success(IpcResponseData::Pong)) =
                    read_framed::<_, IpcResponse>(&mut stream)
            {
                return true;
            }
        }
    }
    false
}

/// Length of the nonces exchanged during the authentication handshake.
pub const AUTH_NONCE_LEN: usize = 32;

/// Length of the authentication MAC.
pub const AUTH_MAC_LEN: usize = 32;

/// Length of the session secret a master publishes to legitimate peers.
pub const AUTH_SECRET_LEN: usize = 32;

/// Domain separator for the authentication transcript.
const AUTH_DOMAIN: &[u8] = b"oifs-ipc-auth-v1";

/// Maximum number of peers that may be connected to a master at the same time.
///
/// Bounds the resource usage of a hostile client that opens connections and
/// never authenticates (SEC-18).
pub const MAX_CONCURRENT_PEERS: usize = 256;

/// Time budget for the authentication handshake of a single connection.
///
/// A peer that connects but never authenticates is dropped after this
/// timeout, so a Slowloris-style client cannot hold a worker thread
/// indefinitely (SEC-18).
pub const AUTH_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Computes the authentication MAC over a transcript with a pre-shared key.
///
/// Keyed BLAKE2b (`Blake2bMac`) is used so the key is mixed into the block
/// function instead of being merely prefixed to the message.
fn auth_mac(key: &[u8], transcript: &[u8]) -> [u8; AUTH_MAC_LEN] {
    use blake2::digest::{FixedOutput, Update};
    let mut mac = blake2::Blake2bMac512::new_with_salt_and_personal(key, &[], AUTH_DOMAIN)
        .expect("the authentication key is shorter than the BLAKE2b block size");
    mac.update(transcript);
    let out = mac.finalize_fixed();
    let mut tag = [0u8; AUTH_MAC_LEN];
    tag.copy_from_slice(&out[..AUTH_MAC_LEN]);
    tag
}

/// Builds the handshake transcript for one direction of the exchange.
///
/// The direction byte keeps the two tags from being interchangeable.
fn auth_transcript(
    direction: u8,
    client_nonce: &[u8; AUTH_NONCE_LEN],
    server_nonce: &[u8; AUTH_NONCE_LEN],
    salt: &[u8; 16],
) -> Vec<u8> {
    let mut transcript = Vec::with_capacity(1 + AUTH_NONCE_LEN + AUTH_NONCE_LEN + 16);
    transcript.push(direction);
    transcript.extend_from_slice(client_nonce);
    transcript.extend_from_slice(server_nonce);
    transcript.extend_from_slice(salt);
    transcript
}

/// Constant-time equality check for authentication tags.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// The credential a client presents during the authentication handshake.
#[derive(Debug, Clone)]
pub enum AuthCredential {
    /// The client knows the filesystem password and derives the same key the
    /// master holds from the password and the salt sent in the challenge.
    Password(String),
    /// The client holds the session secret published by the master.
    Secret([u8; AUTH_SECRET_LEN]),
}

/// Request payloads sent from Client processes to the Master process
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcRequest {
    Ping,
    /// First step of the authentication handshake.
    ///
    /// The client sends a random nonce and *never* a secret: the cleartext
    /// password used to travel over the wire before, which let anyone who
    /// could redirect a client (for example by tampering with the rendezvous
    /// file) harvest it (SEC-18).
    AuthChallenge {
        client_nonce: [u8; AUTH_NONCE_LEN],
        /// Whether the client holds the filesystem password.
        has_password: bool,
    },
    /// The client's answer to the master's challenge.
    AuthResponse {
        client_mac: [u8; AUTH_MAC_LEN],
    },
    CreateFile {
        parent_inode_id: u64,
        name: String,
    },
    CreateDirectory {
        parent_inode_id: u64,
        name: String,
    },
    Lookup {
        parent_inode_id: u64,
        name: String,
    },
    ReadData {
        inode_id: u64,
    },
    ReadAt {
        inode_id: u64,
        file_offset: u64,
        len: usize,
    },
    WriteData {
        inode_id: u64,
        file_offset: u64,
        data: Vec<u8>,
        compression_mode: CompressionMode,
        filter_config: crate::filters::FilterConfig,
    },
    DeleteFile {
        parent_inode_id: u64,
        name: String,
    },
    ResolvePath {
        path: String,
    },
    ResolveParent {
        path: String,
    },
    ReadInode {
        inode_id: u64,
    },
    ListDir {
        dir_inode_id: u64,
    },
    GetSuperblock,
    Flush,
    AnalyzeFragmentation,
    Defragment {
        source_path: String,
        mode: DefragMode,
        output_path: Option<String>,
    },
    VerifyIntegrity,
    /// Upgrade a legacy (v1) image to the current inode format.
    Migrate,
    GetBlockCopy {
        block_id: u64,
    },
    Truncate {
        inode_id: u64,
        new_size: u64,
    },
}

impl IpcRequest {
    /// Returns the short operation name without formatting debug strings or allocating memory
    pub fn name(&self) -> &'static str {
        match self {
            IpcRequest::Ping => "Ping",
            IpcRequest::AuthChallenge { .. } => "AuthChallenge",
            IpcRequest::AuthResponse { .. } => "AuthResponse",
            IpcRequest::CreateFile { .. } => "CreateFile",
            IpcRequest::CreateDirectory { .. } => "CreateDirectory",
            IpcRequest::Lookup { .. } => "Lookup",
            IpcRequest::ReadData { .. } => "ReadData",
            IpcRequest::ReadAt { .. } => "ReadAt",
            IpcRequest::WriteData { .. } => "WriteData",
            IpcRequest::DeleteFile { .. } => "DeleteFile",
            IpcRequest::Truncate { .. } => "Truncate",
            IpcRequest::ResolvePath { .. } => "ResolvePath",
            IpcRequest::ResolveParent { .. } => "ResolveParent",
            IpcRequest::ReadInode { .. } => "ReadInode",
            IpcRequest::ListDir { .. } => "ListDir",
            IpcRequest::GetSuperblock => "GetSuperblock",
            IpcRequest::Flush => "Flush",
            IpcRequest::AnalyzeFragmentation => "AnalyzeFragmentation",
            IpcRequest::Defragment { .. } => "Defragment",
            IpcRequest::VerifyIntegrity => "VerifyIntegrity",
            IpcRequest::Migrate => "Migrate",
            IpcRequest::GetBlockCopy { .. } => "GetBlockCopy",
        }
    }
}

/// Data returned on successful IPC request execution
#[derive(Debug, Serialize, Deserialize)]
pub enum IpcResponseData {
    Pong,
    InodeId(u64),
    Data(Vec<u8>),
    Unit,
    ResolveParent {
        parent_id: u64,
        name: String,
    },
    Inode(Inode),
    DirectoryEntries(Vec<DirectoryEntry>),
    Superblock(SuperBlock),
    Fragmentation(FragmentationStats),
    Defrag(DefragStats),
    Fsck(FsckReport),
    Migration(MigrationStats),
    BlockCopy(Option<Vec<u8>>),
    /// The master's challenge: its nonce, the image's salt and whether the
    /// image is encrypted (which decides which credential the client must use).
    AuthChallenge {
        server_nonce: [u8; AUTH_NONCE_LEN],
        salt: [u8; 16],
        encrypted: bool,
    },
    /// The master's proof that it holds the same key as the client.
    AuthVerify {
        server_mac: [u8; AUTH_MAC_LEN],
    },
}

/// Top-level response frame
#[derive(Debug, Serialize, Deserialize)]
pub enum IpcResponse {
    Success(IpcResponseData),
    Error(String),
}

/// Maximum size of a single IPC frame payload (bytes).
///
/// Frames larger than this are rejected before any allocation happens, so a
/// malicious or buggy peer cannot force the receiver to reserve arbitrary
/// amounts of memory by simply announcing a huge length.
pub const MAX_FRAME_BYTES: usize = 128 * 1024 * 1024;

/// Chunk size used to stream frame payloads off the wire.
const FRAME_READ_CHUNK: usize = 64 * 1024;

/// Returns `true` if an announced frame payload length is within the allowed limit.
///
/// This is the single source of truth for frame-length validation and is used
/// by both the read and the write path *before* any allocation happens.
#[inline]
pub const fn frame_len_is_valid(len: u32) -> bool {
    (len as usize) <= MAX_FRAME_BYTES
}

/// Bincode configuration used for every IPC frame.
///
/// It reproduces the legacy `bincode::serialize_into` / `bincode::deserialize`
/// wire format exactly (fixint encoding, trailing bytes allowed) but adds a
/// hard byte limit. Without the limit, a small frame can announce a `Vec` or
/// `String` length of `u64::MAX` and make the receiver allocate gigabytes
/// before reading a single payload byte (SEC-17).
fn frame_codec(limit: u64) -> impl bincode::Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .allow_trailing_bytes()
        .with_limit(limit)
}

thread_local! {
    /// Reusable scratch buffer for the *write* path. Kept separate from the
    /// read buffer so a thread that writes a response right after reading a
    /// request never needs two buffers at once.
    static FRAME_WRITE_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    /// Reusable scratch buffer for the *read* path.
    static FRAME_READ_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    /// Reusable staging buffer used to stream payload bytes off the wire.
    static FRAME_CHUNK_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Serializes and writes a length-prefixed frame to a stream.
///
/// The serialization buffer is pooled per thread, so steady-state traffic
/// performs zero heap allocations for the frame itself.
pub fn write_framed<W: Write, T: Serialize>(writer: &mut W, val: &T) -> io::Result<()> {
    FRAME_WRITE_BUF.with(|cell| {
        let mut buf = cell.borrow_mut();
        buf.clear();
        buf.extend_from_slice(&[0u8; 4]);
        frame_codec(MAX_FRAME_BYTES as u64)
            .serialize_into(&mut *buf, val)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let payload_len = buf.len() - 4;
        let len = u32::try_from(payload_len).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Message frame exceeds size limit",
            )
        })?;
        if !frame_len_is_valid(len) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Message frame exceeds size limit",
            ));
        }
        buf[..4].copy_from_slice(&len.to_le_bytes());
        writer.write_all(&buf)?;
        writer.flush()?;
        Ok(())
    })
}

/// Reads a length-prefixed frame and deserializes it.
///
/// The payload is streamed in bounded chunks into a pooled buffer: the length
/// prefix is validated *before* allocating, and the buffer capacity is reused
/// across frames instead of being reallocated per message.
pub fn read_framed<R: Read, T: DeserializeOwned>(reader: &mut R) -> io::Result<T> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;

    if !frame_len_is_valid(len as u32) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Message frame exceeds size limit",
        ));
    }

    FRAME_READ_BUF.with(|cell| {
        let mut buf = cell.borrow_mut();
        buf.clear();
        if len > 0 {
            // Reserve progressively; never allocate the announced size blindly.
            buf.reserve(FRAME_READ_CHUNK.min(len));
            let mut remaining = len;
            while remaining > 0 {
                let taken = FRAME_CHUNK_BUF.with(|cell| {
                    let mut stage = cell.borrow_mut();
                    if stage.len() < FRAME_READ_CHUNK {
                        stage.resize(FRAME_READ_CHUNK, 0);
                    }
                    let taken = remaining.min(stage.len());
                    reader.read_exact(&mut stage[..taken])?;
                    buf.extend_from_slice(&stage[..taken]);
                    Ok::<usize, io::Error>(taken)
                })?;
                remaining -= taken;
            }
        }
        // The limit is the size of the frame itself: a payload can never claim
        // more bytes than the frame carries, so a crafted length prefix cannot
        // trigger a huge allocation (SEC-17).
        frame_codec(buf.len() as u64)
            .deserialize(buf.as_slice())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    })
}

/// The result of attempting to acquire master status or connect to an existing master
pub enum MasterOrClient {
    Master(IpcListener),
    Client {
        stream: IpcStream,
        target_path: PathBuf,
        /// The session secret published by the master (empty when the master
        /// could not be reached for it).
        auth_secret: Option<[u8; AUTH_SECRET_LEN]>,
    },
}

fn is_addr_in_use(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::AddrInUse || err.kind() == io::ErrorKind::AlreadyExists {
        return true;
    }
    if let Some(code) = err.raw_os_error()
        && (code == libc::EADDRINUSE || code == libc::EEXIST)
    {
        return true;
    }
    false
}

/// Binds a Unix Domain Socket with owner-only permissions.
///
/// The socket is created inside a `0700` private directory and the socket
/// itself is restricted to `0600`, so only processes of the same user can
/// reach it (SEC-18).
fn bind_unix_listener(socket_path: &Path) -> io::Result<UnixListener> {
    let listener = UnixListener::bind(socket_path)?;
    restrict_permissions(socket_path, 0o600);
    Ok(listener)
}

/// Hex-encodes a session secret.
fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Decodes a hex-encoded session secret.
fn hex_decode(text: &str) -> Option<[u8; AUTH_SECRET_LEN]> {
    if text.len() != AUTH_SECRET_LEN * 2 {
        return None;
    }
    let mut out = [0u8; AUTH_SECRET_LEN];
    for (i, chunk) in text.as_bytes().chunks(2).enumerate() {
        let hex = std::str::from_utf8(chunk).ok()?;
        out[i] = u8::from_str_radix(hex, 16).ok()?;
    }
    Some(out)
}

/// Generates a fresh random session secret.
fn generate_auth_secret() -> [u8; AUTH_SECRET_LEN] {
    let mut secret = [0u8; AUTH_SECRET_LEN];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut secret);
    secret
}

/// Reads the session secret published by a local (UDS) master.
///
/// A master publishes the secret right after it binds its socket, so a client
/// that wins the race may see the socket before the secret exists. The read
/// therefore waits briefly for the file to appear.
fn read_token_file(token_path: &Path) -> Option<[u8; AUTH_SECRET_LEN]> {
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    loop {
        if let Some(secret) = read_token_file_once(token_path) {
            return Some(secret);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Reads the session secret once, without waiting.
fn read_token_file_once(token_path: &Path) -> Option<[u8; AUTH_SECRET_LEN]> {
    let bytes = fs::read(token_path).ok()?;
    if bytes.len() != AUTH_SECRET_LEN {
        return None;
    }
    let mut secret = [0u8; AUTH_SECRET_LEN];
    secret.copy_from_slice(&bytes);
    Some(secret)
}

/// Writes the session secret of a local (UDS) master with owner-only access.
fn write_token_file(token_path: &Path, secret: &[u8; AUTH_SECRET_LEN]) -> io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    let _ = fs::remove_file(token_path);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(token_path)?;
    file.write_all(secret)?;
    file.flush()?;
    restrict_permissions(token_path, 0o600);
    Ok(())
}

/// Attempts to bind as Master or connect as Client according to SessionMode
pub fn bind_or_connect<P: AsRef<Path>>(
    image_path: P,
    mode: &SessionMode,
) -> io::Result<MasterOrClient> {
    match mode {
        SessionMode::Standalone => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "IPC is disabled in Standalone mode",
        )),
        SessionMode::Local => {
            let socket_path = get_socket_path(&image_path);
            let token_path = get_token_path(&image_path);

            if socket_path.exists() {
                for attempt in 0..10 {
                    match UnixStream::connect(&socket_path) {
                        Ok(stream) => {
                            return Ok(MasterOrClient::Client {
                                stream: IpcStream::Unix(stream),
                                target_path: socket_path,
                                auth_secret: read_token_file(&token_path),
                            });
                        }
                        Err(_) => {
                            if attempt < 9 {
                                thread::sleep(Duration::from_millis(15));
                            }
                        }
                    }
                }
                let _ = fs::remove_file(&socket_path);
                let _ = fs::remove_file(&token_path);
            }

            match bind_unix_listener(&socket_path) {
                Ok(listener) => {
                    let auth_secret = generate_auth_secret();
                    write_token_file(&token_path, &auth_secret)?;
                    Ok(MasterOrClient::Master(IpcListener::Unix {
                        listener,
                        socket_path,
                        token_path,
                        auth_secret,
                    }))
                }
                Err(e) if is_addr_in_use(&e) => {
                    for attempt in 0..15 {
                        match UnixStream::connect(&socket_path) {
                            Ok(stream) => {
                                return Ok(MasterOrClient::Client {
                                    stream: IpcStream::Unix(stream),
                                    target_path: socket_path,
                                    auth_secret: read_token_file(&token_path),
                                });
                            }
                            Err(_) => {
                                if attempt < 14 {
                                    thread::sleep(Duration::from_millis(15));
                                }
                            }
                        }
                    }
                    // If connecting still fails because socket became stale/dead, remove and retry bind
                    let _ = fs::remove_file(&socket_path);
                    let _ = fs::remove_file(&token_path);
                    match bind_unix_listener(&socket_path) {
                        Ok(listener) => {
                            let auth_secret = generate_auth_secret();
                            write_token_file(&token_path, &auth_secret)?;
                            Ok(MasterOrClient::Master(IpcListener::Unix {
                                listener,
                                socket_path,
                                token_path,
                                auth_secret,
                            }))
                        }
                        Err(_) => {
                            let stream = UnixStream::connect(&socket_path)?;
                            Ok(MasterOrClient::Client {
                                stream: IpcStream::Unix(stream),
                                target_path: socket_path,
                                auth_secret: read_token_file(&token_path),
                            })
                        }
                    }
                }
                Err(e) => Err(e),
            }
        }
        SessionMode::Network { bind_addr } => {
            let master_path = get_master_info_path(image_path);

            // 1. Check if rendezvous master file exists
            if master_path.exists() {
                if let Ok(content) = fs::read_to_string(&master_path)
                    && let Ok(info) = serde_json::from_str::<MasterInfo>(&content)
                    && is_tcp_master_alive(&info.addr)
                    && let Ok(stream) = TcpStream::connect(&info.addr)
                {
                    let _ = stream.set_nodelay(true);
                    return Ok(MasterOrClient::Client {
                        stream: IpcStream::Tcp(stream),
                        target_path: master_path,
                        auth_secret: hex_decode(&info.auth_token),
                    });
                }
                // Dead master! Clean up stale rendezvous file
                let _ = fs::remove_file(&master_path);
            }

            // 2. Try atomic create_new to become Master
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&master_path)
            {
                Ok(mut file) => {
                    let bind_target = bind_addr.as_deref().unwrap_or("127.0.0.1:0");
                    let listener = TcpListener::bind(bind_target)?;
                    let local_addr = listener.local_addr()?;
                    let auth_secret = generate_auth_secret();

                    let info = MasterInfo {
                        addr: local_addr.to_string(),
                        pid: std::process::id(),
                        auth_token: hex_encode(&auth_secret),
                    };
                    let json = serde_json::to_string(&info)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                    file.write_all(json.as_bytes())?;
                    file.flush()?;
                    // The rendezvous file carries the session secret: it must
                    // not be readable by other users (SEC-18).
                    restrict_permissions(&master_path, 0o600);

                    Ok(MasterOrClient::Master(IpcListener::Tcp {
                        listener,
                        master_file_path: master_path,
                        auth_secret,
                    }))
                }
                Err(e) if is_addr_in_use(&e) => {
                    // Another process won the race. Retry reading info and connecting
                    for attempt in 0..10 {
                        if let Ok(content) = fs::read_to_string(&master_path)
                            && let Ok(info) = serde_json::from_str::<MasterInfo>(&content)
                            && let Ok(stream) = TcpStream::connect(&info.addr)
                        {
                            let _ = stream.set_nodelay(true);
                            return Ok(MasterOrClient::Client {
                                stream: IpcStream::Tcp(stream),
                                target_path: master_path,
                                auth_secret: hex_decode(&info.auth_token),
                            });
                        }
                        if attempt < 9 {
                            thread::sleep(Duration::from_millis(20));
                        }
                    }
                    let content = fs::read_to_string(&master_path)?;
                    let info: MasterInfo = serde_json::from_str(&content)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                    let stream = TcpStream::connect(&info.addr)?;
                    let _ = stream.set_nodelay(true);
                    Ok(MasterOrClient::Client {
                        stream: IpcStream::Tcp(stream),
                        target_path: master_path,
                        auth_secret: hex_decode(&info.auth_token),
                    })
                }
                Err(e) => Err(e),
            }
        }
    }
}

/// Background IPC Server running on the Master process
pub struct IpcServer {
    cleanup_path: PathBuf,
    /// Additional files removed on shutdown (the session secret of a local
    /// master).
    extra_cleanup_paths: Vec<PathBuf>,
    shutdown_signal: Arc<AtomicBool>,
    peer_counter: Arc<AtomicUsize>,
    active_peers: Arc<AtomicUsize>,
    server_thread: Option<JoinHandle<()>>,
}

impl IpcServer {
    /// Starts the background IPC server supporting both Unix and TCP listeners
    pub fn start(
        listener: IpcListener,
        dm: Arc<DiskManager>,
        event_tx: Option<Sender<SessionEvent>>,
    ) -> Self {
        let shutdown_signal = Arc::new(AtomicBool::new(false));
        let peer_counter = Arc::new(AtomicUsize::new(0));
        let active_peers = Arc::new(AtomicUsize::new(0));

        let shutdown_clone = shutdown_signal.clone();
        let peer_counter_clone = peer_counter.clone();
        let active_peers_clone = active_peers.clone();

        let cleanup_path = match &listener {
            IpcListener::Unix { socket_path, .. } => socket_path.clone(),
            IpcListener::Tcp {
                master_file_path, ..
            } => master_file_path.clone(),
        };
        let auth_secret = match &listener {
            IpcListener::Unix { auth_secret, .. } => *auth_secret,
            IpcListener::Tcp { auth_secret, .. } => *auth_secret,
        };
        let extra_cleanup_paths = match &listener {
            IpcListener::Unix { token_path, .. } => vec![token_path.clone()],
            IpcListener::Tcp { .. } => Vec::new(),
        };

        let server_thread = thread::spawn(move || {
            let mut client_threads: Vec<JoinHandle<()>> = Vec::new();

            #[inline]
            fn wait_for_fd_readable(fd: RawFd, timeout_ms: i32) -> bool {
                let mut pfd = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let ret = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
                ret > 0 && (pfd.revents & libc::POLLIN) != 0
            }

            match listener {
                IpcListener::Unix { listener, .. } => {
                    let _ = listener.set_nonblocking(true);
                    let raw_fd = listener.as_raw_fd();
                    while !shutdown_clone.load(Ordering::Relaxed) {
                        client_threads.retain(|h| !h.is_finished());
                        if wait_for_fd_readable(raw_fd, 50) {
                            match listener.accept() {
                                Ok((stream, _)) => {
                                    Self::spawn_client_worker(
                                        IpcStream::Unix(stream),
                                        &dm,
                                        &event_tx,
                                        &peer_counter_clone,
                                        &active_peers_clone,
                                        &mut client_threads,
                                        auth_secret,
                                    );
                                }
                                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                                Err(_) => break,
                            }
                        }
                    }
                }
                IpcListener::Tcp { listener, .. } => {
                    let _ = listener.set_nonblocking(true);
                    let raw_fd = listener.as_raw_fd();
                    while !shutdown_clone.load(Ordering::Relaxed) {
                        client_threads.retain(|h| !h.is_finished());
                        if wait_for_fd_readable(raw_fd, 50) {
                            match listener.accept() {
                                Ok((stream, _)) => {
                                    let _ = stream.set_nodelay(true);
                                    Self::spawn_client_worker(
                                        IpcStream::Tcp(stream),
                                        &dm,
                                        &event_tx,
                                        &peer_counter_clone,
                                        &active_peers_clone,
                                        &mut client_threads,
                                        auth_secret,
                                    );
                                }
                                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                                Err(_) => break,
                            }
                        }
                    }
                }
            }

            for handle in client_threads {
                let _ = handle.join();
            }
        });

        Self {
            cleanup_path,
            extra_cleanup_paths,
            shutdown_signal,
            peer_counter,
            active_peers,
            server_thread: Some(server_thread),
        }
    }

    fn spawn_client_worker(
        mut stream: IpcStream,
        dm: &Arc<DiskManager>,
        event_tx: &Option<Sender<SessionEvent>>,
        peer_counter: &Arc<AtomicUsize>,
        active_peers: &Arc<AtomicUsize>,
        client_threads: &mut Vec<JoinHandle<()>>,
        auth_secret: [u8; AUTH_SECRET_LEN],
    ) {
        // Bound the number of concurrent peers: a client that opens
        // connections without authenticating must not be able to exhaust the
        // process' threads (SEC-18).
        if active_peers.load(Ordering::SeqCst) >= MAX_CONCURRENT_PEERS {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return;
        }

        let peer_id = peer_counter.fetch_add(1, Ordering::SeqCst) + 1;
        active_peers.fetch_add(1, Ordering::SeqCst);

        if let Some(tx) = event_tx {
            let _ = tx.send(SessionEvent::PeerConnected { peer_id });
        }

        let dm_worker = dm.clone();
        let tx_worker = event_tx.clone();
        let active_peers_worker = active_peers.clone();

        let handle = thread::spawn(move || {
            let _ = stream.set_nonblocking(false);
            // The handshake gets a bounded time budget; after it succeeds the
            // connection may idle for as long as the peer keeps it open.
            let _ = stream.set_read_timeout(Some(AUTH_HANDSHAKE_TIMEOUT));
            let _ = stream.set_write_timeout(Some(AUTH_HANDSHAKE_TIMEOUT));

            let is_encrypted = dm_worker.is_encrypted();
            let salt = dm_worker.encryption_salt();
            // The credential the master accepts: the filesystem key for an
            // encrypted image, the published session secret otherwise.
            let master_key: Vec<u8> = if is_encrypted {
                dm_worker.auth_key().unwrap_or_default()
            } else {
                auth_secret.to_vec()
            };

            let mut authenticated = false;
            let mut challenge: Option<([u8; AUTH_NONCE_LEN], [u8; AUTH_NONCE_LEN])> = None;

            while let Ok(req) = read_framed::<_, IpcRequest>(&mut stream) {
                let short_name = req.name().to_string();

                let resp = match req {
                    IpcRequest::Ping => IpcResponse::Success(IpcResponseData::Pong),
                    IpcRequest::AuthChallenge {
                        client_nonce,
                        has_password,
                    } => {
                        if is_encrypted && !has_password {
                            IpcResponse::Error(
                                "Password required to access encrypted filesystem".to_string(),
                            )
                        } else {
                            let mut server_nonce = [0u8; AUTH_NONCE_LEN];
                            rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut server_nonce);
                            challenge = Some((client_nonce, server_nonce));
                            IpcResponse::Success(IpcResponseData::AuthChallenge {
                                server_nonce,
                                salt,
                                encrypted: is_encrypted,
                            })
                        }
                    }
                    IpcRequest::AuthResponse { client_mac } => {
                        match challenge {
                            Some((client_nonce, server_nonce)) => {
                                let expected = auth_mac(
                                    &master_key,
                                    &auth_transcript(0, &client_nonce, &server_nonce, &salt),
                                );
                                if ct_eq(&client_mac, &expected) {
                                    authenticated = true;
                                    // The master proves that it holds the same
                                    // key, so a client that was redirected to a
                                    // fake master detects the redirection.
                                    let server_mac = auth_mac(
                                        &master_key,
                                        &auth_transcript(1, &client_nonce, &server_nonce, &salt),
                                    );
                                    IpcResponse::Success(IpcResponseData::AuthVerify { server_mac })
                                } else if is_encrypted {
                                    IpcResponse::Error(
                                        "Decryption failed: invalid password or credentials"
                                            .to_string(),
                                    )
                                } else {
                                    IpcResponse::Error(
                                        "Authentication failed: invalid credentials".to_string(),
                                    )
                                }
                            }
                            None => {
                                IpcResponse::Error("Unexpected authentication response".to_string())
                            }
                        }
                    }
                    _ if !authenticated => IpcResponse::Error(
                        "Password required to access encrypted filesystem".to_string(),
                    ),
                    _ => {
                        let resp_data = Self::handle_request(&dm_worker, req);
                        match resp_data {
                            Ok(data) => IpcResponse::Success(data),
                            Err(e) => IpcResponse::Error(e.to_string()),
                        }
                    }
                };

                if write_framed(&mut stream, &resp).is_err() {
                    break;
                }

                if authenticated {
                    // The handshake is done: the session may idle indefinitely.
                    let _ = stream.set_read_timeout(None);
                    let _ = stream.set_write_timeout(None);
                }

                if let Some(tx) = &tx_worker {
                    let _ = tx.send(SessionEvent::RequestHandled {
                        peer_id,
                        req_type: short_name,
                    });
                }
            }

            active_peers_worker.fetch_sub(1, Ordering::SeqCst);
            if let Some(tx) = &tx_worker {
                let _ = tx.send(SessionEvent::PeerDisconnected { peer_id });
            }
        });

        client_threads.push(handle);
    }

    /// Returns the number of currently active connected peer processes
    pub fn active_peers_count(&self) -> usize {
        self.active_peers.load(Ordering::SeqCst)
    }

    /// Returns the total number of peer connections accepted
    pub fn total_peers_served(&self) -> usize {
        self.peer_counter.load(Ordering::SeqCst)
    }

    /// Dispatches an incoming request against the local DiskManager
    pub(crate) fn handle_request(
        dm: &DiskManager,
        req: IpcRequest,
    ) -> Result<IpcResponseData, DiskManagerError> {
        match req {
            IpcRequest::Ping => Ok(IpcResponseData::Pong),
            IpcRequest::AuthChallenge { .. } | IpcRequest::AuthResponse { .. } => {
                // The handshake is handled by the connection worker, which owns
                // the per-connection authentication state.
                Err(DiskManagerError::Io(std::io::Error::other(
                    "Unexpected authentication request",
                )))
            }
            IpcRequest::CreateFile {
                parent_inode_id,
                name,
            } => {
                let id = dm.create_file(parent_inode_id, &name)?;
                Ok(IpcResponseData::InodeId(id))
            }
            IpcRequest::CreateDirectory {
                parent_inode_id,
                name,
            } => {
                let id = dm.create_directory(parent_inode_id, &name)?;
                Ok(IpcResponseData::InodeId(id))
            }
            IpcRequest::Lookup {
                parent_inode_id,
                name,
            } => {
                let id = dm.lookup(parent_inode_id, &name)?;
                Ok(IpcResponseData::InodeId(id))
            }
            IpcRequest::ReadData { inode_id } => {
                let data = dm.read_data(inode_id)?;
                Ok(IpcResponseData::Data(data))
            }
            IpcRequest::ReadAt {
                inode_id,
                file_offset,
                len,
            } => {
                let inode = dm.read_inode(inode_id)?;
                let available = inode.size.saturating_sub(file_offset) as usize;
                let alloc_len = len.min(available).min(128 * 1024 * 1024);
                let mut buf = vec![0u8; alloc_len];
                let n = if alloc_len > 0 {
                    dm.read_at(inode_id, file_offset, &mut buf)?
                } else {
                    0
                };
                buf.truncate(n);
                Ok(IpcResponseData::Data(buf))
            }
            IpcRequest::WriteData {
                inode_id,
                file_offset,
                data,
                compression_mode,
                filter_config,
            } => {
                dm.write_data_with_filters(
                    inode_id,
                    file_offset,
                    &data,
                    compression_mode,
                    filter_config,
                )?;
                Ok(IpcResponseData::Unit)
            }
            IpcRequest::DeleteFile {
                parent_inode_id,
                name,
            } => {
                dm.delete_file(parent_inode_id, &name)?;
                Ok(IpcResponseData::Unit)
            }
            IpcRequest::Truncate { inode_id, new_size } => {
                dm.truncate(inode_id, new_size)?;
                Ok(IpcResponseData::Unit)
            }
            IpcRequest::ResolvePath { path } => {
                let id = dm.resolve_path(&path)?;
                Ok(IpcResponseData::InodeId(id))
            }
            IpcRequest::ResolveParent { path } => {
                let (parent_id, name) = dm.resolve_parent(&path)?;
                Ok(IpcResponseData::ResolveParent { parent_id, name })
            }
            IpcRequest::ReadInode { inode_id } => {
                let inode = dm.read_inode(inode_id)?;
                Ok(IpcResponseData::Inode(inode))
            }
            IpcRequest::ListDir { dir_inode_id } => {
                let entries = dm.list_dir(dir_inode_id)?;
                Ok(IpcResponseData::DirectoryEntries(entries))
            }
            IpcRequest::GetSuperblock => {
                let sb = dm.superblock();
                Ok(IpcResponseData::Superblock(sb))
            }
            IpcRequest::Flush => {
                dm.flush()?;
                Ok(IpcResponseData::Unit)
            }
            IpcRequest::AnalyzeFragmentation => {
                let stats = dm.analyze_fragmentation()?;
                Ok(IpcResponseData::Fragmentation(stats))
            }
            IpcRequest::Defragment {
                source_path,
                mode,
                output_path,
            } => {
                let stats = dm.defragment(&source_path, mode, output_path.as_deref())?;
                Ok(IpcResponseData::Defrag(stats))
            }
            IpcRequest::VerifyIntegrity => {
                let report = dm.verify_integrity()?;
                Ok(IpcResponseData::Fsck(report))
            }
            IpcRequest::Migrate => {
                let stats = dm.migrate()?;
                Ok(IpcResponseData::Migration(stats))
            }
            IpcRequest::GetBlockCopy { block_id } => {
                let copy = dm.get_block_copy(block_id);
                Ok(IpcResponseData::BlockCopy(copy))
            }
        }
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        let start = std::time::Instant::now();
        while self.active_peers.load(Ordering::SeqCst) > 0
            && start.elapsed() < Duration::from_millis(1500)
        {
            thread::sleep(Duration::from_millis(10));
        }

        self.shutdown_signal.store(true, Ordering::SeqCst);
        if let Some(thread) = self.server_thread.take() {
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.cleanup_path);
        for path in &self.extra_cleanup_paths {
            let _ = fs::remove_file(path);
        }
    }
}

/// Client IPC proxy for communicating with the Master process
pub struct IpcClient {
    stream: Mutex<IpcStream>,
    target_path: PathBuf,
}

impl IpcClient {
    /// Creates a new IpcClient with an existing IpcStream
    pub fn new(stream: IpcStream, target_path: PathBuf) -> Self {
        Self {
            stream: Mutex::new(stream),
            target_path,
        }
    }

    /// Performs the challenge-response handshake with the Master process.
    ///
    /// The password never travels over the wire: the client proves that it
    /// knows it by answering a fresh challenge with a keyed MAC, and the
    /// master proves the same in return. A client that was redirected to a
    /// fake master therefore neither leaks its password nor talks to the
    /// impostor (SEC-18).
    pub fn authenticate(&self, credential: &AuthCredential) -> Result<(), io::Error> {
        let mut client_nonce = [0u8; AUTH_NONCE_LEN];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut client_nonce);
        let has_password = matches!(credential, AuthCredential::Password(_));

        let challenge = self.send(IpcRequest::AuthChallenge {
            client_nonce,
            has_password,
        })?;
        let (server_nonce, salt, encrypted) = match challenge {
            IpcResponseData::AuthChallenge {
                server_nonce,
                salt,
                encrypted,
            } => (server_nonce, salt, encrypted),
            _ => {
                return Err(io::Error::other("Unexpected authentication response"));
            }
        };

        // The master tells the client which credential it expects: an
        // encrypted image requires the filesystem password, a plain image uses
        // the published session secret.
        let key: Vec<u8> = match credential {
            AuthCredential::Password(password) => {
                if !encrypted {
                    return Err(io::Error::other(
                        "Master does not accept a password for an unencrypted filesystem",
                    ));
                }
                let derived = crate::encryption::derive_key(password, &salt)
                    .map_err(|e| io::Error::other(e.to_string()))?;
                derived.as_bytes().to_vec()
            }
            AuthCredential::Secret(secret) => {
                if encrypted {
                    return Err(io::Error::other(
                        "Password required to access encrypted filesystem",
                    ));
                }
                secret.to_vec()
            }
        };

        let client_mac = auth_mac(
            &key,
            &auth_transcript(0, &client_nonce, &server_nonce, &salt),
        );
        let verified = self.send(IpcRequest::AuthResponse { client_mac })?;
        match verified {
            IpcResponseData::AuthVerify { server_mac } => {
                let expected = auth_mac(
                    &key,
                    &auth_transcript(1, &client_nonce, &server_nonce, &salt),
                );
                if ct_eq(&server_mac, &expected) {
                    Ok(())
                } else {
                    Err(io::Error::other(
                        "Master failed to prove it holds the filesystem key",
                    ))
                }
            }
            _ => Err(io::Error::other("Unexpected authentication response")),
        }
    }

    /// Sends an IPC request to the Master and waits for response
    pub fn send(&self, req: IpcRequest) -> Result<IpcResponseData, io::Error> {
        // Deliberately NOT `lock_unpoisoned`: a panic between `write_framed` and
        // `read_framed` leaves a partially written frame in the stream, so the peer
        // is now out of step. Recovering the lock would let the next caller write
        // into a desynchronized stream and silently corrupt the protocol, which is
        // far worse than propagating the panic.
        let mut guard = self.stream.lock().unwrap();
        write_framed(&mut *guard, &req)?;
        let resp: IpcResponse = read_framed(&mut *guard)?;

        match resp {
            IpcResponse::Success(data) => Ok(data),
            IpcResponse::Error(err_msg) => Err(io::Error::other(err_msg)),
        }
    }

    /// Returns the target socket/master file path
    pub fn target_path(&self) -> &Path {
        &self.target_path
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    /// Prove the frame-length validation invariant (P4.8).
    ///
    /// A frame is acceptable if and only if its announced payload length does
    /// not exceed `MAX_FRAME_BYTES`; the zero-length frame stays valid so that
    /// empty payloads can never wedge the protocol.
    #[kani::proof]
    fn proof_frame_len_validation_soundness() {
        let len: u32 = kani::any();

        let accepted = frame_len_is_valid(len);

        if accepted {
            assert!(
                (len as usize) <= MAX_FRAME_BYTES,
                "Accepted frames must respect the size limit"
            );
        } else {
            assert!(
                (len as usize) > MAX_FRAME_BYTES,
                "Rejected frames must exceed the size limit"
            );
        }

        kani::cover!(accepted, "Valid frame length reached");
        kani::cover!(!accepted, "Oversized frame length rejected");
    }

    /// Prove that a zero-length frame is always accepted (protocol liveness).
    #[kani::proof]
    fn proof_zero_length_frame_is_valid() {
        assert!(frame_len_is_valid(0));
    }
}
