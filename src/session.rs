//! Session module for transparent Master-Proxy coordination
//!
//! Provides `OifsSession`, an abstraction that automatically determines whether
//! to operate in Direct mode (Master: direct `mmap`, zero IPC overhead) or
//! Remote mode (Client: transparent IPC proxy).
//!
//! Supports two transport modes:
//! - `SessionMode::Local`: Default, uses Unix Domain Socket (UDS) in `/tmp`
//! - `SessionMode::Network`: Opt-in, uses Rendezvous `.image.master` file in the image directory
//!   + TCP transport + Clock-Free Active Ping Probe for NFS/Lustre/multi-node clusters.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex, OnceLock};
use thiserror::Error;

use crate::directory::DirectoryEntry;
use crate::disk::{
    CompressionMode, DefragMode, DefragStats, DiskManager, DiskManagerError, FragmentationStats,
    FsckReport,
};
use crate::inode::Inode;
use crate::ipc::{
    IpcClient, IpcRequest, IpcResponseData, IpcServer, MasterOrClient, SessionEvent, SessionMode,
    bind_or_connect,
};
use crate::superblock::SuperBlock;

/// Errors that can occur during session operations
#[derive(Error, Debug)]
pub enum SessionError {
    /// Local DiskManager error
    #[error("DiskManager error: {0}")]
    DiskManager(#[from] DiskManagerError),
    /// Standard I/O error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// Remote error from Master
    #[error("Remote error: {0}")]
    Remote(String),
    /// Unexpected response payload from Master
    #[error("Unexpected IPC response payload")]
    UnexpectedResponse,
}

/// Unified session handle for accessing an OIFS file system
///
/// If this process is the first to open the filesystem, it runs in **Direct (Master)** mode:
/// - Local operations directly access `mmap` with zero IPC overhead.
/// - In the background, an IPC server listens for peer processes.
/// - Emits `SessionEvent` when other processes connect.
///
/// If another process is already accessing the filesystem, it runs in **Remote (Client)** mode:
/// - Requests are transparently forwarded to the Master process over a single connection FD.
/// - Automatically handles seamless failover/reconnection if Master exits.
#[derive(Clone)]
pub enum OifsSession {
    /// Master mode with direct disk access and background IPC server
    Direct {
        dm: Arc<DiskManager>,
        server: Arc<IpcServer>,
        event_rx: Arc<Mutex<Option<Receiver<SessionEvent>>>>,
        mode: SessionMode,
    },
    /// Client mode with transparent IPC forwarding and self-healing failover
    Remote {
        client: Arc<IpcClient>,
        image_path: PathBuf,
        mode: SessionMode,
        password: Option<String>,
        dm_fallback: Arc<Mutex<Option<Arc<DiskManager>>>>,
    },
}

static SESSION_REGISTRY: OnceLock<Mutex<HashMap<PathBuf, OifsSession>>> = OnceLock::new();

fn session_registry() -> &'static Mutex<HashMap<PathBuf, OifsSession>> {
    SESSION_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolves a path to its canonical, absolute representation, following any symbolic links
/// (even if the final target file has not yet been created on disk).
pub(crate) fn canonicalize_path<P: AsRef<Path>>(path: P) -> PathBuf {
    let path_ref = path.as_ref();
    if let Ok(canon) = fs::canonicalize(path_ref) {
        return canon;
    }

    let mut current = if path_ref.is_absolute() {
        path_ref.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path_ref)
    };

    let mut hops = 0;
    while hops < 32 {
        if let Ok(target) = fs::read_link(&current) {
            if target.is_absolute() {
                current = target;
            } else {
                let parent = current.parent().unwrap_or(Path::new("."));
                current = parent.join(target);
            }
            hops += 1;
        } else {
            break;
        }
    }

    if let Ok(canon) = fs::canonicalize(&current) {
        canon
    } else {
        let parent = current.parent().unwrap_or(Path::new(""));
        let parent_canon = if parent.as_os_str().is_empty() {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        } else {
            fs::canonicalize(parent)
                .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        };

        if let Some(name) = current.file_name() {
            parent_canon.join(name)
        } else {
            parent_canon
        }
    }
}

impl OifsSession {
    /// Gets an existing session for the given path from the process-wide registry, or opens a new one (Local UDS mode).
    ///
    /// Automatically resolves symbolic links and relative paths to their canonical form,
    /// ensuring all threads in the same process share the same Master session with zero IPC overhead.
    pub fn get_or_open<P: AsRef<Path>>(path: P, total_size: u64) -> Result<Self, SessionError> {
        Self::get_or_open_with_mode(path, total_size, SessionMode::Local, None, false)
    }

    /// Gets an existing encrypted session or opens a new one with a password using default Local mode (UDS).
    pub fn get_or_open_with_password<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        password: Option<&str>,
    ) -> Result<Self, SessionError> {
        Self::get_or_open_with_mode(path, total_size, SessionMode::Local, password, false)
    }

    /// Gets an existing encrypted session or creates a new one using default Local mode (UDS).
    pub fn get_or_create_encrypted<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        password: &str,
    ) -> Result<Self, SessionError> {
        Self::get_or_open_with_mode(path, total_size, SessionMode::Local, Some(password), true)
    }

    /// Gets an existing session in Network mode or opens a new one.
    pub fn get_or_open_network<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        bind_addr: Option<String>,
    ) -> Result<Self, SessionError> {
        Self::get_or_open_with_mode(
            path,
            total_size,
            SessionMode::Network { bind_addr },
            None,
            false,
        )
    }

    /// Gets an existing encrypted session in Network mode or opens a new one with a password.
    pub fn get_or_open_network_with_password<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        bind_addr: Option<String>,
        password: Option<&str>,
    ) -> Result<Self, SessionError> {
        Self::get_or_open_with_mode(
            path,
            total_size,
            SessionMode::Network { bind_addr },
            password,
            false,
        )
    }

    /// Gets an existing session or opens a new one with full configuration mode.
    ///
    /// Thread-safe: Concurrent calls from multiple threads for the same file (or symlinks pointing
    /// to the same file) will safely synchronize and return clones of the single Master session.
    pub fn get_or_open_with_mode<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        mode: SessionMode,
        password: Option<&str>,
        create_encrypted: bool,
    ) -> Result<Self, SessionError> {
        let canon_key = canonicalize_path(path.as_ref());
        let registry = session_registry();
        let mut guard = registry.lock().unwrap();

        if let Some(session) = guard.get(&canon_key) {
            return Ok(session.clone());
        }

        let session =
            Self::open_with_mode(&canon_key, total_size, mode, password, create_encrypted)?;
        guard.insert(canon_key, session.clone());
        Ok(session)
    }

    /// Removes a path from the process-wide session registry if present.
    pub fn unregister_from_registry<P: AsRef<Path>>(path: P) -> Option<OifsSession> {
        let canon_key = canonicalize_path(path.as_ref());
        let registry = session_registry();
        let mut guard = registry.lock().unwrap();
        guard.remove(&canon_key)
    }

    /// Clears all cached sessions from the process-wide registry.
    pub fn clear_registry() {
        let registry = session_registry();
        let mut guard = registry.lock().unwrap();
        guard.clear();
    }

    /// Opens an existing filesystem or creates a new one using default Local mode (UDS)
    pub fn open<P: AsRef<Path>>(path: P, total_size: u64) -> Result<Self, SessionError> {
        Self::open_with_mode(path, total_size, SessionMode::Local, None, false)
    }

    /// Opens an encrypted filesystem with a password using default Local mode (UDS)
    pub fn open_with_password<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        password: Option<&str>,
    ) -> Result<Self, SessionError> {
        Self::open_with_mode(path, total_size, SessionMode::Local, password, false)
    }

    /// Creates a new encrypted filesystem using default Local mode (UDS)
    pub fn create_encrypted<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        password: &str,
    ) -> Result<Self, SessionError> {
        Self::open_with_mode(path, total_size, SessionMode::Local, Some(password), true)
    }

    /// Opens an existing filesystem in Network mode (TCP + Rendezvous File)
    pub fn open_network<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        bind_addr: Option<String>,
    ) -> Result<Self, SessionError> {
        Self::open_with_mode(
            path,
            total_size,
            SessionMode::Network { bind_addr },
            None,
            false,
        )
    }

    /// Opens an encrypted filesystem in Network mode (TCP + Rendezvous File)
    pub fn open_network_with_password<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        bind_addr: Option<String>,
        password: Option<&str>,
    ) -> Result<Self, SessionError> {
        Self::open_with_mode(
            path,
            total_size,
            SessionMode::Network { bind_addr },
            password,
            false,
        )
    }

    /// Creates an encrypted filesystem in Network mode (TCP + Rendezvous File)
    pub fn create_encrypted_network<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        bind_addr: Option<String>,
        password: &str,
    ) -> Result<Self, SessionError> {
        Self::open_with_mode(
            path,
            total_size,
            SessionMode::Network { bind_addr },
            Some(password),
            true,
        )
    }

    /// Unified session constructor
    pub fn open_with_mode<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        mode: SessionMode,
        password: Option<&str>,
        create_encrypted: bool,
    ) -> Result<Self, SessionError> {
        Self::open_with_mode_retry(
            path.as_ref(),
            total_size,
            mode,
            password,
            create_encrypted,
            0,
        )
    }

    fn open_with_mode_retry(
        path_ref: &Path,
        total_size: u64,
        mode: SessionMode,
        password: Option<&str>,
        create_encrypted: bool,
        retry_count: usize,
    ) -> Result<Self, SessionError> {
        let master_or_client = match bind_or_connect(path_ref, &mode) {
            Ok(res) => res,
            Err(_e) if retry_count < 5 => {
                std::thread::sleep(std::time::Duration::from_millis(
                    20 * (retry_count + 1) as u64,
                ));
                return Self::open_with_mode_retry(
                    path_ref,
                    total_size,
                    mode,
                    password,
                    create_encrypted,
                    retry_count + 1,
                );
            }
            Err(e) => return Err(e.into()),
        };

        match master_or_client {
            MasterOrClient::Master(listener) => {
                let dm_res = if create_encrypted {
                    DiskManager::create_encrypted(
                        path_ref,
                        total_size,
                        password.unwrap_or_default(),
                    )
                } else if let Some(pwd) = password {
                    DiskManager::open_with_password(path_ref, total_size, Some(pwd))
                } else {
                    DiskManager::open(path_ref, total_size)
                };

                let dm = match dm_res {
                    Ok(d) => d,
                    Err(DiskManagerError::Locking(errno)) => {
                        drop(listener);
                        if retry_count < 5 {
                            std::thread::sleep(std::time::Duration::from_millis(
                                25 * (retry_count + 1) as u64,
                            ));
                            return Self::open_with_mode_retry(
                                path_ref,
                                total_size,
                                mode,
                                password,
                                create_encrypted,
                                retry_count + 1,
                            );
                        } else {
                            return Err(DiskManagerError::Locking(errno).into());
                        }
                    }
                    Err(e) => return Err(e.into()),
                };

                let dm_arc = Arc::new(dm);
                let (tx, rx) = channel();
                let server = IpcServer::start(listener, dm_arc.clone(), Some(tx));

                Ok(OifsSession::Direct {
                    dm: dm_arc,
                    server: Arc::new(server),
                    event_rx: Arc::new(Mutex::new(Some(rx))),
                    mode,
                })
            }
            MasterOrClient::Client {
                stream,
                target_path,
            } => {
                let client = IpcClient::new(stream, target_path);
                Ok(OifsSession::Remote {
                    client: Arc::new(client),
                    image_path: path_ref.to_path_buf(),
                    mode,
                    password: password.map(|s| s.to_string()),
                    dm_fallback: Arc::new(Mutex::new(None)),
                })
            }
        }
    }

    /// Returns `true` if this session is the Master (Direct mode), `false` if Client (Remote mode)
    pub fn is_direct(&self) -> bool {
        matches!(self, OifsSession::Direct { .. })
    }

    /// Returns the session mode (Local or Network)
    pub fn mode(&self) -> &SessionMode {
        match self {
            OifsSession::Direct { mode, .. } => mode,
            OifsSession::Remote { mode, .. } => mode,
        }
    }

    /// Returns the number of currently active peer processes connected to this Master
    pub fn peer_count(&self) -> usize {
        match self {
            OifsSession::Direct { server, .. } => server.active_peers_count(),
            OifsSession::Remote { .. } => 0,
        }
    }

    /// Returns the total number of peer connections accepted by this Master
    pub fn total_peers_served(&self) -> usize {
        match self {
            OifsSession::Direct { server, .. } => server.total_peers_served(),
            OifsSession::Remote { .. } => 0,
        }
    }

    /// Takes the event receiver (available on Direct Master session)
    pub fn take_event_receiver(&self) -> Option<Receiver<SessionEvent>> {
        match self {
            OifsSession::Direct { event_rx, .. } => {
                let mut guard = event_rx.lock().unwrap();
                guard.take()
            }
            OifsSession::Remote { .. } => None,
        }
    }

    /// Sends an IPC request with automatic failover/reconnection if the Master exits
    fn send_request(&self, req: IpcRequest) -> Result<IpcResponseData, SessionError> {
        match self {
            OifsSession::Direct { dm, .. } => {
                IpcServer::handle_request(dm, req).map_err(SessionError::DiskManager)
            }
            OifsSession::Remote {
                client,
                image_path,
                mode,
                password,
                dm_fallback,
            } => {
                // 1. If we already fell back to a local DiskManager:
                {
                    let guard = dm_fallback.lock().unwrap();
                    if let Some(ref local_dm) = *guard {
                        return IpcServer::handle_request(local_dm, req)
                            .map_err(SessionError::DiskManager);
                    }
                }

                // 2. Retry loop for high-concurrency master handovers
                let mut current_client = client.clone();
                for attempt in 0..5 {
                    match current_client.send(req.clone()) {
                        Ok(data) => return Ok(data),
                        Err(io_err) if Self::is_connection_error(&io_err) => {
                            if attempt == 4 {
                                return Err(SessionError::Remote(io_err.to_string()));
                            }
                            std::thread::sleep(std::time::Duration::from_millis(
                                15 * (attempt + 1) as u64,
                            ));

                            let retry_session = Self::open_with_mode(
                                image_path,
                                0,
                                mode.clone(),
                                password.as_deref(),
                                false,
                            )?;

                            match retry_session {
                                OifsSession::Direct { dm, .. } => {
                                    let mut guard = dm_fallback.lock().unwrap();
                                    *guard = Some(dm.clone());
                                    return IpcServer::handle_request(&dm, req)
                                        .map_err(SessionError::DiskManager);
                                }
                                OifsSession::Remote {
                                    client: new_client, ..
                                } => {
                                    current_client = new_client;
                                }
                            }
                        }
                        Err(other) => return Err(SessionError::Remote(other.to_string())),
                    }
                }

                Err(SessionError::Remote("Max retry attempts exceeded".into()))
            }
        }
    }

    fn is_connection_error(err: &std::io::Error) -> bool {
        if matches!(
            err.kind(),
            std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::NotConnected
        ) {
            return true;
        }

        let msg = err.to_string();
        msg.contains("Broken pipe")
            || msg.contains("Connection reset")
            || msg.contains("failed to fill whole buffer")
            || msg.contains("Connection refused")
    }

    /// Creates a new file in a directory
    pub fn create_file(&self, parent_inode_id: u64, name: &str) -> Result<u64, SessionError> {
        match self {
            OifsSession::Direct { dm, .. } => Ok(dm.create_file(parent_inode_id, name)?),
            OifsSession::Remote { .. } => {
                let resp = self.send_request(IpcRequest::CreateFile {
                    parent_inode_id,
                    name: name.to_string(),
                })?;

                match resp {
                    IpcResponseData::InodeId(id) => Ok(id),
                    _ => Err(SessionError::UnexpectedResponse),
                }
            }
        }
    }

    /// Creates a new directory in a parent directory
    pub fn create_directory(&self, parent_inode_id: u64, name: &str) -> Result<u64, SessionError> {
        match self {
            OifsSession::Direct { dm, .. } => Ok(dm.create_directory(parent_inode_id, name)?),
            OifsSession::Remote { .. } => {
                let resp = self.send_request(IpcRequest::CreateDirectory {
                    parent_inode_id,
                    name: name.to_string(),
                })?;

                match resp {
                    IpcResponseData::InodeId(id) => Ok(id),
                    _ => Err(SessionError::UnexpectedResponse),
                }
            }
        }
    }

    /// Looks up a file/directory by name within a parent directory
    pub fn lookup(&self, parent_inode_id: u64, name: &str) -> Result<u64, SessionError> {
        match self {
            OifsSession::Direct { dm, .. } => Ok(dm.lookup(parent_inode_id, name)?),
            OifsSession::Remote { .. } => {
                let resp = self.send_request(IpcRequest::Lookup {
                    parent_inode_id,
                    name: name.to_string(),
                })?;

                match resp {
                    IpcResponseData::InodeId(id) => Ok(id),
                    _ => Err(SessionError::UnexpectedResponse),
                }
            }
        }
    }

    /// Reads data from a file
    pub fn read_data(&self, inode_id: u64) -> Result<Vec<u8>, SessionError> {
        match self {
            OifsSession::Direct { dm, .. } => Ok(dm.read_data(inode_id)?),
            OifsSession::Remote { .. } => {
                let resp = self.send_request(IpcRequest::ReadData { inode_id })?;

                match resp {
                    IpcResponseData::Data(data) => Ok(data),
                    _ => Err(SessionError::UnexpectedResponse),
                }
            }
        }
    }

    /// Writes data to a file (default: no pre-compression filters)
    pub fn write_data(
        &self,
        inode_id: u64,
        file_offset: u64,
        data: &[u8],
        compression_mode: CompressionMode,
    ) -> Result<(), SessionError> {
        self.write_data_with_filters(
            inode_id,
            file_offset,
            data,
            compression_mode,
            crate::filters::FilterConfig::none(),
        )
    }

    /// Writes data to a file with custom pre-compression filters
    pub fn write_data_with_filters(
        &self,
        inode_id: u64,
        file_offset: u64,
        data: &[u8],
        compression_mode: CompressionMode,
        filter_config: crate::filters::FilterConfig,
    ) -> Result<(), SessionError> {
        match self {
            OifsSession::Direct { dm, .. } => Ok(dm.write_data_with_filters(
                inode_id,
                file_offset,
                data,
                compression_mode,
                filter_config,
            )?),
            OifsSession::Remote { .. } => {
                let resp = self.send_request(IpcRequest::WriteData {
                    inode_id,
                    file_offset,
                    data: data.to_vec(),
                    compression_mode,
                    filter_config,
                })?;

                match resp {
                    IpcResponseData::Unit => Ok(()),
                    _ => Err(SessionError::UnexpectedResponse),
                }
            }
        }
    }

    /// Deletes a file from a directory
    pub fn delete_file(&self, parent_inode_id: u64, name: &str) -> Result<(), SessionError> {
        match self {
            OifsSession::Direct { dm, .. } => Ok(dm.delete_file(parent_inode_id, name)?),
            OifsSession::Remote { .. } => {
                let resp = self.send_request(IpcRequest::DeleteFile {
                    parent_inode_id,
                    name: name.to_string(),
                })?;

                match resp {
                    IpcResponseData::Unit => Ok(()),
                    _ => Err(SessionError::UnexpectedResponse),
                }
            }
        }
    }

    /// Resolves a path (e.g. "docs/file.txt") to an Inode ID
    pub fn resolve_path(&self, path: &str) -> Result<u64, SessionError> {
        match self {
            OifsSession::Direct { dm, .. } => Ok(dm.resolve_path(path)?),
            OifsSession::Remote { .. } => {
                let resp = self.send_request(IpcRequest::ResolvePath {
                    path: path.to_string(),
                })?;

                match resp {
                    IpcResponseData::InodeId(id) => Ok(id),
                    _ => Err(SessionError::UnexpectedResponse),
                }
            }
        }
    }

    /// Resolves the parent directory inode ID and filename component for a path
    pub fn resolve_parent(&self, path: &str) -> Result<(u64, String), SessionError> {
        match self {
            OifsSession::Direct { dm, .. } => Ok(dm.resolve_parent(path)?),
            OifsSession::Remote { .. } => {
                let resp = self.send_request(IpcRequest::ResolveParent {
                    path: path.to_string(),
                })?;

                match resp {
                    IpcResponseData::ResolveParent { parent_id, name } => Ok((parent_id, name)),
                    _ => Err(SessionError::UnexpectedResponse),
                }
            }
        }
    }

    /// Reads an inode from the inode table
    pub fn read_inode(&self, inode_id: u64) -> Result<Inode, SessionError> {
        match self {
            OifsSession::Direct { dm, .. } => Ok(dm.read_inode(inode_id)?),
            OifsSession::Remote { .. } => {
                let resp = self.send_request(IpcRequest::ReadInode { inode_id })?;

                match resp {
                    IpcResponseData::Inode(inode) => Ok(inode),
                    _ => Err(SessionError::UnexpectedResponse),
                }
            }
        }
    }

    /// Lists all entries in a directory
    pub fn list_dir(&self, dir_inode_id: u64) -> Result<Vec<DirectoryEntry>, SessionError> {
        match self {
            OifsSession::Direct { dm, .. } => Ok(dm.list_dir(dir_inode_id)?),
            OifsSession::Remote { .. } => {
                let resp = self.send_request(IpcRequest::ListDir { dir_inode_id })?;

                match resp {
                    IpcResponseData::DirectoryEntries(entries) => Ok(entries),
                    _ => Err(SessionError::UnexpectedResponse),
                }
            }
        }
    }

    /// Gets a copy of the SuperBlock
    pub fn superblock(&self) -> Result<SuperBlock, SessionError> {
        match self {
            OifsSession::Direct { dm, .. } => Ok(dm.superblock()),
            OifsSession::Remote { .. } => {
                let resp = self.send_request(IpcRequest::GetSuperblock)?;

                match resp {
                    IpcResponseData::Superblock(sb) => Ok(sb),
                    _ => Err(SessionError::UnexpectedResponse),
                }
            }
        }
    }

    /// Flushes any pending memory-mapped changes to disk
    pub fn flush(&self) -> Result<(), SessionError> {
        match self {
            OifsSession::Direct { dm, .. } => Ok(dm.flush()?),
            OifsSession::Remote { .. } => {
                let resp = self.send_request(IpcRequest::Flush)?;

                match resp {
                    IpcResponseData::Unit => Ok(()),
                    _ => Err(SessionError::UnexpectedResponse),
                }
            }
        }
    }

    /// Analyzes disk fragmentation
    pub fn analyze_fragmentation(&self) -> Result<FragmentationStats, SessionError> {
        match self {
            OifsSession::Direct { dm, .. } => Ok(dm.analyze_fragmentation()?),
            OifsSession::Remote { .. } => {
                let resp = self.send_request(IpcRequest::AnalyzeFragmentation)?;

                match resp {
                    IpcResponseData::Fragmentation(stats) => Ok(stats),
                    _ => Err(SessionError::UnexpectedResponse),
                }
            }
        }
    }

    /// Defragments the filesystem
    pub fn defragment(
        &self,
        source_path: &str,
        mode: DefragMode,
        output_path: Option<&str>,
    ) -> Result<DefragStats, SessionError> {
        match self {
            OifsSession::Direct { dm, .. } => Ok(dm.defragment(source_path, mode, output_path)?),
            OifsSession::Remote { .. } => {
                let resp = self.send_request(IpcRequest::Defragment {
                    source_path: source_path.to_string(),
                    mode,
                    output_path: output_path.map(|s| s.to_string()),
                })?;

                match resp {
                    IpcResponseData::Defrag(stats) => Ok(stats),
                    _ => Err(SessionError::UnexpectedResponse),
                }
            }
        }
    }

    /// Verifies structural filesystem integrity (fsck)
    pub fn verify_integrity(&self) -> Result<FsckReport, SessionError> {
        match self {
            OifsSession::Direct { dm, .. } => Ok(dm.verify_integrity()?),
            OifsSession::Remote { .. } => {
                let resp = self.send_request(IpcRequest::VerifyIntegrity)?;

                match resp {
                    IpcResponseData::Fsck(report) => Ok(report),
                    _ => Err(SessionError::UnexpectedResponse),
                }
            }
        }
    }

    /// Reads a copy of a raw block (used by debug and low level tools)
    pub fn get_block_copy(&self, block_id: u64) -> Result<Option<Vec<u8>>, SessionError> {
        match self {
            OifsSession::Direct { dm, .. } => Ok(dm.get_block_copy(block_id)),
            OifsSession::Remote { .. } => {
                let resp = self.send_request(IpcRequest::GetBlockCopy { block_id })?;

                match resp {
                    IpcResponseData::BlockCopy(copy) => Ok(copy),
                    _ => Err(SessionError::UnexpectedResponse),
                }
            }
        }
    }
}
