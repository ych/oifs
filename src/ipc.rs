//! IPC communication module for OIFS file system
//!
//! Provides dual-mode inter-process communication:
//! 1. Default Local Mode: Unix Domain Socket (UDS) based in `/tmp`
//! 2. Optional Network Mode: Rendezvous File (`.image.master`) in the image directory
//!    + TCP Transport + Clock-Free Active Ping Probe for NFS/Lustre/multi-node clusters.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::directory::DirectoryEntry;
use crate::disk::{
    CompressionMode, DefragMode, DefragStats, DiskManager, DiskManagerError,
    FragmentationStats, FsckReport,
};
use crate::inode::Inode;
use crate::superblock::SuperBlock;

/// Session transport mode
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SessionMode {
    /// Default: Local machine only, using Unix Domain Socket (UDS) in `/tmp`
    #[default]
    Local,
    /// Network / Cluster mode: uses Rendezvous master file in the image directory + TCP transport + Active Ping Probe
    Network {
        /// Custom bind address (e.g. "0.0.0.0:9050" or "127.0.0.1:0")
        bind_addr: Option<String>,
    },
}

/// Metadata stored in the `.image.master` rendezvous file for Network Mode
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MasterInfo {
    pub addr: String,
    pub pid: u32,
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
}

/// Abstract Listener supporting both Unix Domain Sockets and TCP
pub enum IpcListener {
    Unix {
        listener: UnixListener,
        socket_path: PathBuf,
    },
    Tcp {
        listener: TcpListener,
        master_file_path: PathBuf,
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

/// Computes a unique, deterministic Unix Domain Socket path for a given image file
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
        .collect();

    std::env::temp_dir().join(format!("oifs_{}_{:016x}.sock", safe_name, hash))
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

/// Request payloads sent from Client processes to the Master process
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcRequest {
    Ping,
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
    GetBlockCopy {
        block_id: u64,
    },
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
    BlockCopy(Option<Vec<u8>>),
}

/// Top-level response frame
#[derive(Debug, Serialize, Deserialize)]
pub enum IpcResponse {
    Success(IpcResponseData),
    Error(String),
}

/// Serializes and writes a length-prefixed frame to a stream
pub fn write_framed<W: Write, T: Serialize>(writer: &mut W, val: &T) -> io::Result<()> {
    let bytes =
        bincode::serialize(val).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let len = bytes.len() as u32;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

/// Reads a length-prefixed frame and deserializes it
pub fn read_framed<R: Read, T: DeserializeOwned>(reader: &mut R) -> io::Result<T> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;

    if len > 128 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Message frame exceeds size limit",
        ));
    }

    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    bincode::deserialize(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// The result of attempting to acquire master status or connect to an existing master
pub enum MasterOrClient {
    Master(IpcListener),
    Client {
        stream: IpcStream,
        target_path: PathBuf,
    },
}

fn is_addr_in_use(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::AddrInUse || err.kind() == io::ErrorKind::AlreadyExists {
        return true;
    }
    if let Some(code) = err.raw_os_error()
        && (code == libc::EADDRINUSE || code == libc::EEXIST) {
            return true;
        }
    false
}

/// Attempts to bind as Master or connect as Client according to SessionMode
pub fn bind_or_connect<P: AsRef<Path>>(
    image_path: P,
    mode: &SessionMode,
) -> io::Result<MasterOrClient> {
    match mode {
        SessionMode::Local => {
            let socket_path = get_socket_path(image_path);

            if socket_path.exists() {
                for attempt in 0..10 {
                    match UnixStream::connect(&socket_path) {
                        Ok(stream) => {
                            return Ok(MasterOrClient::Client {
                                stream: IpcStream::Unix(stream),
                                target_path: socket_path,
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
            }

            match UnixListener::bind(&socket_path) {
                Ok(listener) => Ok(MasterOrClient::Master(IpcListener::Unix {
                    listener,
                    socket_path,
                })),
                Err(e) if is_addr_in_use(&e) => {
                    for attempt in 0..15 {
                        match UnixStream::connect(&socket_path) {
                            Ok(stream) => {
                                return Ok(MasterOrClient::Client {
                                    stream: IpcStream::Unix(stream),
                                    target_path: socket_path,
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
                    match UnixListener::bind(&socket_path) {
                        Ok(listener) => Ok(MasterOrClient::Master(IpcListener::Unix {
                            listener,
                            socket_path,
                        })),
                        Err(_) => {
                            let stream = UnixStream::connect(&socket_path)?;
                            Ok(MasterOrClient::Client {
                                stream: IpcStream::Unix(stream),
                                target_path: socket_path,
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
                    return Ok(MasterOrClient::Client {
                        stream: IpcStream::Tcp(stream),
                        target_path: master_path,
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
                    let bind_target = bind_addr
                        .as_deref()
                        .unwrap_or("127.0.0.1:0");
                    let listener = TcpListener::bind(bind_target)?;
                    let local_addr = listener.local_addr()?;

                    let info = MasterInfo {
                        addr: local_addr.to_string(),
                        pid: std::process::id(),
                    };
                    let json = serde_json::to_string(&info).map_err(|e| {
                        io::Error::new(io::ErrorKind::InvalidData, e.to_string())
                    })?;
                    file.write_all(json.as_bytes())?;
                    file.flush()?;

                    Ok(MasterOrClient::Master(IpcListener::Tcp {
                        listener,
                        master_file_path: master_path,
                    }))
                }
                Err(e) if is_addr_in_use(&e) => {
                    // Another process won the race. Retry reading info and connecting
                    for attempt in 0..10 {
                        if let Ok(content) = fs::read_to_string(&master_path)
                            && let Ok(info) = serde_json::from_str::<MasterInfo>(&content)
                            && let Ok(stream) = TcpStream::connect(&info.addr)
                        {
                            return Ok(MasterOrClient::Client {
                                stream: IpcStream::Tcp(stream),
                                target_path: master_path,
                            });
                        }
                        if attempt < 9 {
                            thread::sleep(Duration::from_millis(20));
                        }
                    }
                    let content = fs::read_to_string(&master_path)?;
                    let info: MasterInfo = serde_json::from_str(&content).map_err(|e| {
                        io::Error::new(io::ErrorKind::InvalidData, e.to_string())
                    })?;
                    let stream = TcpStream::connect(&info.addr)?;
                    Ok(MasterOrClient::Client {
                        stream: IpcStream::Tcp(stream),
                        target_path: master_path,
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

        let server_thread = thread::spawn(move || {
            let mut client_threads: Vec<JoinHandle<()>> = Vec::new();

            match listener {
                IpcListener::Unix { listener, .. } => {
                    let _ = listener.set_nonblocking(true);
                    while !shutdown_clone.load(Ordering::Relaxed) {
                        client_threads.retain(|h| !h.is_finished());
                        match listener.accept() {
                            Ok((stream, _)) => {
                                Self::spawn_client_worker(
                                    IpcStream::Unix(stream),
                                    &dm,
                                    &event_tx,
                                    &peer_counter_clone,
                                    &active_peers_clone,
                                    &mut client_threads,
                                );
                            }
                            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                                thread::sleep(Duration::from_millis(2));
                            }
                            Err(_) => break,
                        }
                    }
                }
                IpcListener::Tcp { listener, .. } => {
                    let _ = listener.set_nonblocking(true);
                    while !shutdown_clone.load(Ordering::Relaxed) {
                        client_threads.retain(|h| !h.is_finished());
                        match listener.accept() {
                            Ok((stream, _)) => {
                                Self::spawn_client_worker(
                                    IpcStream::Tcp(stream),
                                    &dm,
                                    &event_tx,
                                    &peer_counter_clone,
                                    &active_peers_clone,
                                    &mut client_threads,
                                );
                            }
                            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                                thread::sleep(Duration::from_millis(2));
                            }
                            Err(_) => break,
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
    ) {
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
            while let Ok(req) = read_framed::<_, IpcRequest>(&mut stream) {
                let req_name = format!("{:?}", req);
                let short_name = req_name
                    .split('{')
                    .next()
                    .unwrap_or("Req")
                    .trim()
                    .to_string();

                let resp_data = Self::handle_request(&dm_worker, req);
                let resp = match resp_data {
                    Ok(data) => IpcResponse::Success(data),
                    Err(e) => IpcResponse::Error(e.to_string()),
                };

                if write_framed(&mut stream, &resp).is_err() {
                    break;
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
            IpcRequest::WriteData {
                inode_id,
                file_offset,
                data,
                compression_mode,
                filter_config,
            } => {
                dm.write_data_with_filters(inode_id, file_offset, &data, compression_mode, filter_config)?;
                Ok(IpcResponseData::Unit)
            }
            IpcRequest::DeleteFile {
                parent_inode_id,
                name,
            } => {
                dm.delete_file(parent_inode_id, &name)?;
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

    /// Sends an IPC request to the Master and waits for response
    pub fn send(&self, req: IpcRequest) -> Result<IpcResponseData, io::Error> {
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
