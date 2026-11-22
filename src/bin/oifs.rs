use clap::{Parser, Subcommand};
use oifs::auto_defrag::AutoDefragConfig;
use oifs::disk::{CompressionMode, DefragMode};
use oifs::inode::FileType;
use oifs::session::OifsSession;
use serde::Serialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use zeroize::{Zeroize, Zeroizing};

/// Formats a Unix timestamp (seconds) as `YYYY-MM-DD HH:MM:SS` in the local time zone.
fn format_local_timestamp(secs: u64) -> String {
    let t = libc::time_t::try_from(secs).unwrap_or(libc::time_t::MAX);
    // SAFETY: `tm` is a plain C struct (all-zero is valid) and `localtime_r` is the
    // thread-safe variant that only writes into the caller-provided buffer.
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return secs.to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year as i64 + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// Write end of the self-pipe used to forward SIGINT/SIGTERM to the handler thread.
static INTERRUPT_PIPE_WR: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

extern "C" fn on_interrupt_signal(_sig: libc::c_int) {
    let fd = INTERRUPT_PIPE_WR.load(std::sync::atomic::Ordering::Acquire);
    if fd >= 0 {
        let byte = 1u8;
        // SAFETY: `write(2)` is async-signal-safe; the result is intentionally ignored.
        unsafe {
            libc::write(fd, (&byte as *const u8).cast(), 1);
        }
    }
}

/// RAII guard for the process interrupt-handler thread.
///
/// Dropping this guard closes the self-pipe write end and joins the signal thread so
/// any `OifsSession` / `Arc<DiskManager>` captured by the cleanup closure is released
/// *before* the command's own session drops (ensuring `IpcServer::drop` drains peers
/// cleanly instead of leaking until process exit).
struct InterruptGuard {
    wr_fd: libc::c_int,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        INTERRUPT_PIPE_WR.store(-1, std::sync::atomic::Ordering::Release);
        if self.wr_fd >= 0 {
            // SAFETY: `wr_fd` is owned exclusively by this guard; closing it wakes
            // the signal thread's blocking `read(2)` with EOF (`0`).
            unsafe {
                libc::close(self.wr_fd);
            }
            self.wr_fd = -1;
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

/// Runs `handler` on a dedicated thread when the process receives SIGINT or SIGTERM.
///
/// The signal handler itself only writes one byte into a self-pipe (async-signal-safe);
/// the cleanup closure (which may take locks and `msync`) runs on a normal thread.
/// Hold the returned [`InterruptGuard`] for the lifetime of the guarded operation.
fn set_interrupt_handler<F>(handler: F) -> std::io::Result<InterruptGuard>
where
    F: FnOnce() + Send + 'static,
{
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a valid two-element buffer for `pipe(2)`.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let (rd, wr) = (fds[0], fds[1]);
    for fd in [rd, wr] {
        // SAFETY: `fd` was just returned by `pipe(2)`.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    INTERRUPT_PIPE_WR.store(wr, std::sync::atomic::Ordering::Release);

    let thread = std::thread::Builder::new()
        .name("oifs-signal".into())
        .spawn(move || {
            let mut byte = 0u8;
            loop {
                // SAFETY: reads at most one byte into a valid stack buffer.
                let n = unsafe { libc::read(rd, (&mut byte as *mut u8).cast(), 1) };
                if n == 1 {
                    // SAFETY: `rd` is owned exclusively by this thread.
                    unsafe { libc::close(rd) };
                    handler();
                    return;
                }
                if n == 0
                    || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                {
                    // SAFETY: `rd` is owned exclusively by this thread.
                    unsafe { libc::close(rd) };
                    return;
                }
            }
        })?;

    for sig in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: installs a handler that only performs an async-signal-safe `write(2)`.
        unsafe {
            let mut action = std::mem::zeroed::<libc::sigaction>();
            action.sa_sigaction = on_interrupt_signal as *const () as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(sig, &action, std::ptr::null_mut()) != 0 {
                let err = std::io::Error::last_os_error();
                let _guard = InterruptGuard {
                    wr_fd: wr,
                    thread: Some(thread),
                };
                return Err(err);
            }
        }
    }
    Ok(InterruptGuard {
        wr_fd: wr,
        thread: Some(thread),
    })
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[arg(short, long)]
    image: Option<PathBuf>,

    /// Password for encrypted filesystem (optional, will prompt if needed)
    #[arg(short, long)]
    password: Option<String>,

    /// Enable local IPC (Inter-Process Communication) mode for multi-process concurrency
    #[arg(long = "ipc", global = true)]
    ipc: bool,

    /// Enable cross-machine / network mode (for NFS, Lustre, or multi-node clusters)
    #[arg(short = 'n', long = "network", global = true)]
    network: bool,

    /// Custom bind address for network mode (e.g. 0.0.0.0:9050 or 127.0.0.1:0).
    ///
    /// The IPC transport is plain TCP without TLS, so a non-loopback
    /// address has to be requested explicitly with `--allow-remote-ipc`.
    #[arg(long = "bind", global = true)]
    bind: Option<String>,

    /// Allow the IPC master to bind a non-loopback address.
    ///
    /// Only safe when the traffic is tunnelled (WireGuard, IPsec, SSH port
    /// forwarding); the IPC transport itself is not encrypted.
    #[arg(long = "allow-remote-ipc", alias = "allow-remote-ipsc", global = true)]
    allow_remote_ipc: bool,

    /// Output results as minified JSON
    #[arg(long, global = true)]
    json: bool,

    /// Enable the background automatic defragmentation coordinator.
    ///
    /// Optionally takes the fragmentation threshold above which a
    /// defragmentation pass is triggered. The ratio is
    /// (free_runs - 1) / (free_blocks - 1): 0.0 means all free space is
    /// contiguous, 1.0 means every free block is isolated. Defaults to 0.25.
    #[arg(long, global = true, value_name = "THRESHOLD", num_args = 0..=1, default_missing_value = "0.25")]
    auto_defrag: Option<f64>,

    /// Interval in seconds between background fragmentation checks.
    #[arg(long, global = true, default_value_t = 300)]
    defrag_interval: u64,

    /// Use pure positional pread/pwrite file I/O instead of mmap (eliminates SIGBUS and mmap hazards)
    #[arg(long = "no-mmap", global = true)]
    no_mmap: bool,

    /// Open the image in read-only mode (shared F_RDLCK lock, zero writes, works on 0444 images)
    #[arg(long = "read-only", global = true)]
    read_only: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List files in the root directory or specified path
    Ls {
        /// Path to list (optional, defaults to root)
        path: Option<String>,
        /// List recursively
        #[arg(short, long)]
        recursive: bool,
    },
    /// Create a new OIFS image
    Create {
        /// Size in MB
        #[arg(long, default_value_t = 10)]
        size: u64,
        /// Enable encryption (will prompt for password)
        #[arg(long)]
        encrypt: bool,
        /// Enable the metadata WAL (journaling) for crash-safe metadata updates.
        ///
        /// Reserves 33 blocks (1 header + 128 KB ring) between the data bitmap and
        /// the inode table. Uncommitted transactions are replayed automatically on
        /// the next mount, so `oifs fsck` is no longer needed after a power loss.
        #[arg(long)]
        journal: bool,
    },
    /// Import a file into the image
    Put {
        /// File path on host
        host_path: PathBuf,
        /// Destination filename in OIFS (optional, defaults to host filename)
        remote_name: Option<String>,
        /// Always compress the file regardless of size
        #[arg(long)]
        compress: bool,
        /// Never compress the file
        #[arg(long)]
        no_compress: bool,
        /// Seekable 64KB chunked compression
        #[arg(long, aliases = ["seekable", "chunked-64k"])]
        chunked: bool,
        /// Pre-compression filter to apply: none, delta, shuffle, both (numeric), or auto
        #[arg(long, default_value = "none")]
        filter: String,
        /// Element size in bytes for filter (1, 2, 4, 8)
        #[arg(long, default_value_t = 0)]
        typesize: u8,
    },
    /// Analyze a file and recommend optimal blosc2-style compression filters
    FilterAnalyze {
        /// File path on host to analyze
        host_path: PathBuf,
    },
    /// Export a file from the image
    Get {
        /// Filename in OIFS
        remote_name: String,
        /// Destination path on host (optional, defaults to remote filename)
        host_path: Option<PathBuf>,
    },
    /// Print file contents from the image to stdout
    Cat {
        /// Filename in OIFS
        remote_name: String,
    },
    /// Display filesystem superblock and configuration information
    Info,
    /// Display inode metadata for a file or directory
    Stat {
        /// Path in OIFS
        path: String,
    },
    /// Append content to a file in the image
    Append {
        /// Destination filename in OIFS
        remote_name: String,
        /// Content to append
        content: String,
        /// Do not append a newline automatically
        #[arg(long)]
        no_newline: bool,
    },
    /// Create a directory
    Mkdir {
        /// Directory name
        dir_name: String,
    },
    /// Delete a file or directory
    Rm {
        /// Path of the file or directory to delete
        path: String,
        /// Allow deleting a directory that still has contents
        #[arg(short, long)]
        recursive: bool,
    },
    /// Analyze disk fragmentation
    Analyze,
    /// Defragment the filesystem
    Defrag {
        /// Defragmentation mode (safe or inplace)
        #[arg(long, default_value = "safe")]
        mode: String,
    },
    /// Check filesystem structural consistency (fsck)
    Fsck,
    /// Upgrade a legacy image to the current inode format
    Migrate,
    /// Truncate or extend a file to the specified size
    Truncate {
        /// Remote path in OIFS
        path: String,
        /// New size in bytes
        #[arg(short, long)]
        size: u64,
    },
    /// Mount the OIFS image as a FUSE filesystem
    Mount {
        /// Directory where the filesystem will be mounted
        mountpoint: PathBuf,
        /// Mount backend: `auto` (macOS: nfs, Linux: fuse), `nfs` (built-in local NFSv3, zero kext), or `fuse`
        #[arg(long, default_value = "auto")]
        backend: String,
        /// Loopback TCP port for NFSv3 backend (0 = OS-assigned ephemeral port)
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Run attached in the foreground and log mount details to stderr
        #[arg(long)]
        foreground: bool,
        /// Allow other users to access the mounted filesystem (`allow_other`, FUSE backend)
        #[arg(long)]
        allow_other: bool,
    },
    /// Clone an entire OIFS image using OS CoW reflink (APFS clonefile / Linux FICLONE)
    Clone {
        /// Source OIFS image path (defaults to --image if omitted)
        src: PathBuf,
        /// Destination OIFS image path (must not already exist)
        dst: PathBuf,
    },
    /// Fast cross-image file copy or move (3-tier: FICLONERANGE -> copy_file_range -> raw blocks)
    Transfer {
        /// Destination OIFS image path
        #[arg(long = "to")]
        to: PathBuf,
        /// Source file path inside the source image (--image)
        src_path: String,
        /// Destination file path inside --to image (defaults to src_path)
        #[arg(long = "dest")]
        dest: Option<String>,
        /// Remove source file after destination commits and flushes
        #[arg(long = "move")]
        move_file: bool,
        /// Overwrite existing file at destination
        #[arg(long)]
        overwrite: bool,
        /// Password for encrypted destination image (optional)
        #[arg(long = "to-password")]
        to_password: Option<String>,
        /// Sub-range byte offset in source file (enables `splice_range_to`)
        #[arg(long = "offset")]
        offset: Option<u64>,
        /// Sub-range byte length to splice (defaults to remaining source bytes from --offset)
        #[arg(long = "len")]
        len: Option<u64>,
        /// Sub-range byte offset in destination file (defaults to --offset or 0)
        #[arg(long = "dst-offset")]
        dst_offset: Option<u64>,
        /// Source trim mode for sub-range splice: keep, punch-hole, or collapse
        #[arg(long = "trim-mode", default_value = "keep")]
        trim_mode: String,
    },
}

/// Helper function to read password securely from stdin (with terminal echo masked).
///
/// Prompt goes to stderr so stdout remains usable for JSON pipelines.
fn read_password(prompt: &str) -> Result<Zeroizing<String>, Box<dyn std::error::Error>> {
    use std::io::{self, Write};
    eprint!("{}", prompt);
    io::stderr().flush()?;

    let isatty = unsafe { libc::isatty(libc::STDIN_FILENO) } != 0;
    let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
    let mut original_termios = termios;

    if isatty {
        unsafe {
            if libc::tcgetattr(libc::STDIN_FILENO, &mut termios) == 0 {
                original_termios = termios;
                termios.c_lflag &= !libc::ECHO;
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &termios);
            }
        }
    }

    let mut password = String::new();
    let read_result = io::stdin().read_line(&mut password);

    if isatty {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original_termios);
            eprintln!(); // Print the newline that was suppressed by disabling ECHO
        }
    }

    read_result?;
    // Keep the plaintext password in a zeroizing buffer; the raw
    // `read_line` scratch string is wiped before the trimmed copy is returned.
    let trimmed = password.trim().to_string();
    password.zeroize();
    Ok(Zeroizing::new(trimmed))
}

/// Helper function to open OifsSession with auto-detection of encrypted filesystems and SessionMode.
///
/// Password precedence: `--password` flag > `OIFS_PASSWORD` env > interactive prompt
/// (interactive prompt is suppressed in `--json` mode to keep stdout pure).
fn open_session(
    image_path: &PathBuf,
    password_arg: &Option<String>,
    mode: &oifs::ipc::SessionMode,
    json_mode: bool,
    auto_defrag: Option<&AutoDefragConfig>,
    read_only: bool,
) -> Result<OifsSession, Box<dyn std::error::Error>> {
    let resolve_password = || -> Result<Zeroizing<String>, Box<dyn std::error::Error>> {
        let password = if let Some(pwd) = password_arg {
            Zeroizing::new(pwd.clone())
        } else if let Ok(env_pwd) = std::env::var("OIFS_PASSWORD") {
            Zeroizing::new(env_pwd)
        } else if json_mode {
            return Err(
                "Encrypted filesystem requires --password or OIFS_PASSWORD env in --json mode"
                    .into(),
            );
        } else {
            read_password("🔒 Encrypted filesystem detected. Enter password: ")?
        };

        if password.is_empty() {
            return Err("Password cannot be empty".into());
        }
        Ok(password)
    };

    if read_only {
        let storage_mode = oifs::storage::default_storage_mode();
        let dm = match oifs::disk::DiskManager::open_read_only_with_storage_mode(
            image_path,
            None,
            storage_mode,
        ) {
            Ok(dm) => dm,
            Err(oifs::disk::DiskManagerError::PasswordRequired) => {
                let password = resolve_password()?;
                oifs::disk::DiskManager::open_read_only_with_storage_mode(
                    image_path,
                    Some(&password),
                    storage_mode,
                )?
            }
            Err(e) => return Err(e.into()),
        };

        let session = OifsSession::Direct {
            dm: std::sync::Arc::new(dm),
            server: None,
            event_rx: std::sync::Arc::new(std::sync::Mutex::new(None)),
            mode: oifs::ipc::SessionMode::Standalone,
        };

        if !json_mode && session.needs_migration()? {
            eprintln!(
                "⚠️  This image uses the legacy inode format (v1). It is fully supported, but \
                 run `oifs -i {} migrate` to upgrade to v{}.",
                image_path.display(),
                oifs::superblock::SuperBlock::FORMAT_VERSION_FIXED_INODE
            );
        }

        if auto_defrag.is_some() && !json_mode {
            eprintln!("⚠️  Background auto-defragmentation disabled in --read-only mode");
        }

        return Ok(session);
    }

    let session = match OifsSession::open_with_mode(image_path, 0, mode.clone(), None, false) {
        Ok(s) => s,
        Err(oifs::session::SessionError::DiskManager(
            oifs::disk::DiskManagerError::PasswordRequired,
        )) => {
            let password = resolve_password()?;
            OifsSession::open_with_mode(image_path, 0, mode.clone(), Some(&password), false)
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?
        }
        Err(e) => return Err(e.into()),
    };

    // A legacy image stays fully usable, but says so once rather than silently
    // running the slower, older encoding forever.
    if !json_mode && session.needs_migration()? {
        eprintln!(
            "⚠️  This image uses the legacy inode format (v1). It is fully supported, but \
             run `oifs -i {} migrate` to upgrade to v{}.",
            image_path.display(),
            oifs::superblock::SuperBlock::FORMAT_VERSION_FIXED_INODE
        );
    }

    // A proxy (client) session does not own the DiskManager, so the coordinator
    // can only run on the master. Warn instead of failing the whole command.
    if let Some(config) = auto_defrag
        && let Err(e) = session.set_auto_defrag(config.clone())
        && !json_mode
    {
        eprintln!("⚠️  Background auto-defragmentation not enabled: {e}");
    }

    Ok(session)
}

fn main() {
    let cli = Cli::parse();
    let is_json = cli.json;
    if let Err(e) = run_cli(cli) {
        if is_json {
            println!("{}", json!({"ok": false, "error": e.to_string()}));
        } else {
            eprintln!("Error: {}", e);
        }
        std::process::exit(1);
    }
}

#[derive(Serialize)]
struct LsEntry {
    name: String,
    kind: String,
    size: u64,
    comp_size: Option<u64>,
    modified: String,
}

fn run_cli(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    if cli.no_mmap {
        oifs::storage::set_default_storage_mode(oifs::StorageMode::FileIo);
    }
    // Background auto-defragmentation configuration (`--auto-defrag[=THRESHOLD]`).
    let auto_defrag: Option<AutoDefragConfig> = cli.auto_defrag.map(|threshold| {
        AutoDefragConfig::new(threshold)
            .with_check_interval(std::time::Duration::from_secs(cli.defrag_interval.max(1)))
    });
    // Refuse to expose the plain-TCP IPC master to other machines
    // unless the operator asks for it explicitly.
    if let Some(bind) = cli.bind.as_deref()
        && !oifs::ipc::is_loopback_bind(bind)
        && !cli.allow_remote_ipc
    {
        return Err(format!(
            "refusing to bind the IPC master to the non-loopback address {bind}: the IPC \
             transport is plain TCP without TLS. Use --allow-remote-ipc only when the \
             traffic is tunnelled (WireGuard/IPsec/SSH)."
        )
        .into());
    }

    let session_mode = if cli.network || cli.bind.is_some() {
        oifs::ipc::SessionMode::Network {
            bind_addr: cli.bind.clone(),
        }
    } else if cli.ipc {
        oifs::ipc::SessionMode::Local
    } else {
        oifs::ipc::SessionMode::Standalone
    };

    if let Commands::FilterAnalyze { host_path } = &cli.command {
        if !host_path.exists() {
            return Err(format!("Host file {:?} does not exist.", host_path).into());
        }
        let content = std::fs::read(host_path)?;
        let rec = oifs::filters::recommend_filters(&content);
        if cli.json {
            println!("{}", serde_json::to_string_pretty(&rec)?);
        } else {
            println!(
                "=== OIFS Filter Recommendation Report for {:?} ===",
                host_path
            );
            println!("Original Size:        {} bytes", rec.original_size);
            println!(
                "Baseline Zstd Size:   {} bytes (Entropy: {:.3} bits/byte)",
                rec.baseline_compressed_size, rec.baseline_entropy
            );
            println!("{:-<80}", "");
            println!(
                "{:<32} {:>10} {:>10} {:>10} {:>10}",
                "Filter Pipeline", "Entropy", "Zstd Size", "Ratio", "Savings"
            );
            println!("{:-<80}", "");
            for c in &rec.candidates {
                let marker = if c.config == rec.best_config {
                    " [*RECOMMENDED*]"
                } else {
                    ""
                };
                println!(
                    "{:<32} {:>10.3} {:>10} {:>9.2}x {:>9.1}%{}",
                    c.label,
                    c.entropy,
                    c.compressed_size,
                    c.compression_ratio,
                    c.space_savings_percent,
                    marker
                );
            }
            println!("{:-<80}", "");
            let filter_arg = if rec.best_config.delta && rec.best_config.shuffle {
                "both"
            } else if rec.best_config.delta {
                "delta"
            } else if rec.best_config.shuffle {
                "shuffle"
            } else if rec.best_config.bitshuffle {
                "bitshuffle"
            } else {
                "none"
            };

            let rationale = if rec.best_config.delta {
                "Data displays strong linear/temporal correlation; first-order delta collapses dynamic range, shrinking entropy."
            } else if rec.best_config.bitshuffle {
                "Data displays sparse bits, boolean masks, or low entropy per bit; bit-level shuffle aggregates identical bits."
            } else if rec.best_config.shuffle {
                "Data displays structured record alignment (AoS); byte shuffle groups identical significance bytes into contiguous runs."
            } else {
                "Data appears high-entropy or unstructured; raw compression without pre-filtering is optimal."
            };

            let blosc_equivalents: Vec<&str> = if rec.best_config.delta && rec.best_config.shuffle {
                vec!["blosc2::Filter::Delta", "blosc2::Filter::ByteShuffle"]
            } else if rec.best_config.delta {
                vec!["blosc2::Filter::Delta"]
            } else if rec.best_config.bitshuffle {
                vec!["blosc2::Filter::BitShuffle"]
            } else if rec.best_config.shuffle {
                vec!["blosc2::Filter::ByteShuffle"]
            } else {
                vec!["blosc2::Filter::None"]
            };

            println!("Recommendation Rationale: {}", rationale);
            println!("Recommended Blosc2 Filter(s): {:?}", blosc_equivalents);
            println!("Command to import with recommended filter:");
            println!(
                "  oifs -i <image.img> put {:?} --filter {} --typesize {}",
                host_path, filter_arg, rec.best_config.typesize
            );
        }
        return Ok(());
    }

    if let Commands::Clone { src, dst } = &cli.command {
        let method = oifs::DiskManager::clone_image(src, dst)?;
        if cli.json {
            println!(
                "{}",
                json!({
                    "ok": true,
                    "src": src.to_string_lossy(),
                    "dst": dst.to_string_lossy(),
                    "method": format!("{method:?}"),
                })
            );
        } else {
            println!("Cloned {:?} -> {:?} ({:?})", src, dst, method);
        }
        return Ok(());
    }

    let image = cli
        .image
        .as_ref()
        .ok_or("Image path must be provided via -i / --image")?;

    match &cli.command {
        Commands::FilterAnalyze { .. } | Commands::Clone { .. } => unreachable!(),
        Commands::Create {
            size,
            encrypt,
            journal,
        } => {
            if cli.read_only {
                return Err(oifs::disk::DiskManagerError::ReadOnly.into());
            }
            if image.exists() {
                return Err(format!("Image {:?} already exists.", image).into());
            }
            let size_bytes = size * 1024 * 1024;

            if *encrypt {
                let password = if let Some(pwd) = cli.password.clone() {
                    if pwd.is_empty() {
                        return Err("Password cannot be empty".into());
                    }
                    Zeroizing::new(pwd)
                } else if let Ok(env_pwd) = std::env::var("OIFS_PASSWORD") {
                    if env_pwd.is_empty() {
                        return Err("Password cannot be empty".into());
                    }
                    Zeroizing::new(env_pwd)
                } else if cli.json {
                    return Err(
                        "Encrypted create requires --password or OIFS_PASSWORD env in --json mode"
                            .into(),
                    );
                } else {
                    let pwd = read_password("Enter password: ")?;
                    if pwd.is_empty() {
                        return Err("Password cannot be empty".into());
                    }
                    let pwd_confirm = read_password("Confirm password: ")?;
                    if pwd != pwd_confirm {
                        return Err("Passwords do not match".into());
                    }
                    pwd
                };

                if password.len() < 8 && !cli.json {
                    eprintln!("⚠️  Warning: Password is shorter than 8 characters");
                }

                let _session = if *journal {
                    OifsSession::open_journaled_with_mode(
                        image,
                        size_bytes,
                        session_mode,
                        Some(&password),
                        true,
                    )?
                } else {
                    OifsSession::open_with_mode(
                        image,
                        size_bytes,
                        session_mode,
                        Some(&password),
                        true,
                    )?
                };
                if cli.json {
                    println!(
                        "{}",
                        json!({"ok": true, "message": "Encrypted filesystem created", "journal": journal})
                    );
                } else {
                    println!("✅ Encrypted filesystem created: {:?}", image);
                }
            } else {
                let _session = if *journal {
                    OifsSession::open_journaled_with_mode(
                        image,
                        size_bytes,
                        session_mode,
                        None,
                        false,
                    )?
                } else {
                    OifsSession::open_with_mode(image, size_bytes, session_mode, None, false)?
                };
                if cli.json {
                    println!(
                        "{}",
                        json!({"ok": true, "message": "Filesystem created", "journal": journal})
                    );
                } else {
                    println!(
                        "Created image {:?} with size {}MB{}",
                        image,
                        size,
                        if *journal {
                            " (journaling enabled)"
                        } else {
                            ""
                        }
                    );
                }
            }
            Ok(())
        }
        Commands::Put {
            host_path,
            remote_name,
            compress,
            no_compress,
            chunked,
            filter,
            typesize,
        } => {
            if cli.read_only {
                return Err(oifs::disk::DiskManagerError::ReadOnly.into());
            }
            if !image.exists() {
                return Err(format!("Image {:?} does not exist.", image).into());
            }
            if !host_path.exists() {
                return Err(format!("Host file {:?} does not exist.", host_path).into());
            }

            let path_str = match remote_name {
                Some(name) => name.clone(),
                None => host_path
                    .file_name()
                    .and_then(|f| f.to_str())
                    .ok_or_else(|| {
                        format!(
                            "Cannot determine destination filename from host path {:?}",
                            host_path
                        )
                    })?
                    .to_string(),
            };

            let compression_mode = if *chunked {
                CompressionMode::Seekable {
                    chunk_size: 64 * 1024,
                    level: 3,
                }
            } else if *compress {
                CompressionMode::Always
            } else if *no_compress {
                CompressionMode::Never
            } else {
                CompressionMode::Auto
            };

            let dm = open_session(
                image,
                &cli.password,
                &session_mode,
                cli.json,
                auto_defrag.as_ref(),
                cli.read_only,
            )?;

            let dm_clone = dm.clone();
            let _interrupt_guard = set_interrupt_handler(move || {
                let _ = dm_clone.flush();
                std::process::exit(1);
            })?;

            let (parent_id, filename) = dm.resolve_parent(&path_str)?;

            if dm.lookup(parent_id, &filename).is_ok() {
                return Err(format!("File '{}' already exists.", filename).into());
            }

            let inode_id = dm.create_file(parent_id, &filename)?;
            let content = std::fs::read(host_path)?;

            let filter_config = match filter.to_lowercase().as_str() {
                "auto" => {
                    let rec = oifs::filters::recommend_filters(&content);
                    if !cli.json {
                        println!("Auto-selected filter: {} (entropy: {:.2} -> {:.2}, saving: {:.1}%)",
                            rec.best_report.label, rec.baseline_entropy, rec.best_report.entropy, rec.best_report.space_savings_percent);
                    }
                    rec.best_config
                }
                "delta" => oifs::filters::FilterConfig::delta_only(if *typesize == 0 { 4 } else { *typesize }),
                "shuffle" => oifs::filters::FilterConfig::shuffle_only(if *typesize == 0 { 4 } else { *typesize }),
                "bitshuffle" => oifs::filters::FilterConfig::bitshuffle_only(if *typesize == 0 { 4 } else { *typesize }),
                "both" | "numeric" => oifs::filters::FilterConfig::numeric(if *typesize == 0 { 4 } else { *typesize }),
                "none" | "" => oifs::filters::FilterConfig::none(),
                other => return Err(format!("Unknown filter mode '{}'. Supported: none, delta, shuffle, bitshuffle, both, auto", other).into()),
            };

            dm.write_data_with_filters(inode_id, 0, &content, compression_mode, filter_config)?;
            dm.flush()?;

            if cli.json {
                println!(
                    "{}",
                    json!({"ok": true, "inode": inode_id, "bytes": content.len()})
                );
            } else {
                println!("Imported '{}' to image.", path_str);
            }
            Ok(())
        }
        Commands::Append {
            remote_name,
            content,
            no_newline,
        } => {
            if cli.read_only {
                return Err(oifs::disk::DiskManagerError::ReadOnly.into());
            }
            if !image.exists() {
                return Err(format!("Image {:?} does not exist.", image).into());
            }
            let dm = open_session(
                image,
                &cli.password,
                &session_mode,
                cli.json,
                auto_defrag.as_ref(),
                cli.read_only,
            )?;

            let dm_clone = dm.clone();
            let _interrupt_guard = set_interrupt_handler(move || {
                let _ = dm_clone.flush();
                std::process::exit(1);
            })?;

            let (parent_id, filename) = dm.resolve_parent(remote_name)?;

            let inode_id = match dm.lookup(parent_id, &filename) {
                Ok(id) => id,
                Err(_) => dm.create_file(parent_id, &filename)?,
            };

            let mut existing = dm.read_data(inode_id).unwrap_or_default();
            existing.extend_from_slice(content.as_bytes());
            if !*no_newline && !content.ends_with('\n') {
                existing.push(b'\n');
            }

            dm.write_data(inode_id, 0, &existing, CompressionMode::Auto)?;
            dm.flush()?;

            if cli.json {
                println!(
                    "{}",
                    json!({"ok": true, "inode": inode_id, "total_bytes": existing.len()})
                );
            } else {
                println!(
                    "Appended to '{}'. Total bytes: {}",
                    remote_name,
                    existing.len()
                );
            }
            Ok(())
        }
        Commands::Get {
            remote_name,
            host_path,
        } => {
            if !image.exists() {
                return Err(format!("Image {:?} does not exist.", image).into());
            }
            let dm = open_session(
                image,
                &cli.password,
                &session_mode,
                cli.json,
                auto_defrag.as_ref(),
                cli.read_only,
            )?;
            let inode_id = dm.resolve_path(remote_name)?;
            let data = dm.read_data(inode_id)?;
            let remote_file_name = Path::new(remote_name)
                .file_name()
                .and_then(|f| f.to_str())
                .ok_or_else(|| {
                    format!(
                        "Cannot determine valid filename from remote path '{}'",
                        remote_name
                    )
                })?;

            oifs::directory::validate_filename(remote_file_name)
                .map_err(|e| format!("Invalid remote filename '{}': {}", remote_name, e))?;

            let is_target_dir = match host_path {
                Some(p) => {
                    p.is_dir()
                        || p.to_string_lossy().ends_with('/')
                        || p.to_string_lossy().ends_with('\\')
                }
                None => false,
            };

            let dest = match host_path {
                Some(p) if is_target_dir => p.join(remote_file_name),
                Some(p) => p.clone(),
                None => PathBuf::from(remote_file_name),
            };

            if let Some(parent) = dest.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, &data)?;

            if cli.json {
                println!(
                    "{}",
                    json!({"ok": true, "bytes": data.len(), "dest": dest.to_string_lossy()})
                );
            } else {
                println!("Exported '{}' to {:?}", remote_name, dest);
            }
            Ok(())
        }
        Commands::Cat { remote_name } => {
            if !image.exists() {
                return Err(format!("Image {:?} does not exist.", image).into());
            }
            let dm = open_session(
                image,
                &cli.password,
                &session_mode,
                cli.json,
                auto_defrag.as_ref(),
                cli.read_only,
            )?;
            let inode_id = dm.resolve_path(remote_name)?;
            let data = dm.read_data(inode_id)?;
            if cli.json {
                let text = String::from_utf8_lossy(&data);
                println!(
                    "{}",
                    json!({"ok": true, "path": remote_name, "bytes": data.len(), "content": text})
                );
            } else {
                use std::io::Write;
                let mut stdout = std::io::stdout().lock();
                stdout.write_all(&data)?;
                stdout.flush()?;
            }
            Ok(())
        }
        Commands::Info => {
            if !image.exists() {
                return Err(format!("Image {:?} does not exist.", image).into());
            }
            let dm = open_session(
                image,
                &cli.password,
                &session_mode,
                cli.json,
                auto_defrag.as_ref(),
                cli.read_only,
            )?;
            let sb = dm.superblock()?;
            let total_bytes = sb.block_count.saturating_mul(sb.block_size as u64);
            let journaled = sb.has_journal_layout();
            if cli.json {
                println!(
                    "{}",
                    json!({
                        "ok": true,
                        "image": image.to_string_lossy(),
                        "block_size": sb.block_size,
                        "block_count": sb.block_count,
                        "total_bytes": total_bytes,
                        "inode_count": sb.inode_count,
                        "data_block_start": sb.data_block_start,
                        "format_version": sb.inode_record_version(),
                        "encrypted": sb.encrypted,
                        "journaled": journaled,
                        "read_only": cli.read_only,
                    })
                );
            } else {
                println!("=== OIFS Image Information ===");
                println!("Image:          {}", image.display());
                println!("Block Size:     {} bytes", sb.block_size);
                println!("Total Blocks:   {} ({} bytes)", sb.block_count, total_bytes);
                println!("Inode Capacity: {}", sb.inode_count);
                println!("Format Version: v{}", sb.inode_record_version());
                println!("Encrypted:      {}", sb.encrypted);
                println!("Journaled:      {}", journaled);
                println!("Read-Only:      {}", cli.read_only);
            }
            Ok(())
        }
        Commands::Stat { path } => {
            if !image.exists() {
                return Err(format!("Image {:?} does not exist.", image).into());
            }
            let dm = open_session(
                image,
                &cli.password,
                &session_mode,
                cli.json,
                auto_defrag.as_ref(),
                cli.read_only,
            )?;
            let inode_id = if path == "/" || path.is_empty() {
                dm.superblock()?.root_inode
            } else {
                dm.resolve_path(path)?
            };
            let inode = dm.read_inode(inode_id)?;
            let kind = if inode.mode == FileType::Directory {
                "directory"
            } else {
                "file"
            };
            let seekable = (inode.flags & oifs::inode::INODE_FLAG_SEEKABLE_64K) != 0;
            let created = format_local_timestamp(inode.created_at);
            let modified = format_local_timestamp(inode.modified_at);
            if cli.json {
                println!(
                    "{}",
                    json!({
                        "ok": true,
                        "path": path,
                        "inode": inode_id,
                        "kind": kind,
                        "size": inode.size,
                        "compressed_size": inode.compressed_size,
                        "encrypted": inode.encrypted,
                        "seekable_64k": seekable,
                        "created": created,
                        "modified": modified,
                    })
                );
            } else {
                println!("Path:            {}", path);
                println!("Inode:           {}", inode_id);
                println!("Type:            {}", kind);
                println!("Size:            {} bytes", inode.size);
                println!("Compressed Size: {} bytes", inode.compressed_size);
                println!("Encrypted:       {}", inode.encrypted);
                println!("Seekable 64K:    {}", seekable);
                println!("Created:         {}", created);
                println!("Modified:        {}", modified);
            }
            Ok(())
        }
        Commands::Mkdir { dir_name } => {
            if cli.read_only {
                return Err(oifs::disk::DiskManagerError::ReadOnly.into());
            }
            if !image.exists() {
                return Err(format!("Image {:?} does not exist.", image).into());
            }
            let dm = open_session(
                image,
                &cli.password,
                &session_mode,
                cli.json,
                auto_defrag.as_ref(),
                cli.read_only,
            )?;
            let (parent_id, filename) = dm.resolve_parent(dir_name)?;

            if dm.lookup(parent_id, &filename).is_ok() {
                return Err(format!("Directory or file '{}' already exists.", dir_name).into());
            }

            let dir_id = dm.create_directory(parent_id, &filename)?;
            dm.flush()?;
            if cli.json {
                println!("{}", json!({"ok": true, "inode": dir_id}));
            } else {
                println!("Created directory '{}'", dir_name);
            }
            Ok(())
        }
        Commands::Rm { path, recursive } => {
            if cli.read_only {
                return Err(oifs::disk::DiskManagerError::ReadOnly.into());
            }
            if !image.exists() {
                return Err(format!("Image {:?} does not exist.", image).into());
            }
            let dm = open_session(
                image,
                &cli.password,
                &session_mode,
                cli.json,
                auto_defrag.as_ref(),
                cli.read_only,
            )?;
            let (parent_id, filename) = dm.resolve_parent(path)?;

            let target = match dm.lookup(parent_id, &filename) {
                Ok(id) => id,
                Err(_) => {
                    return Err(format!("File or directory '{}' not found.", path).into());
                }
            };

            // Never let the root directory itself be removed.
            let root = dm.superblock()?.root_inode;
            if target == root {
                return Err("Refusing to delete the filesystem root.".into());
            }

            let is_dir = dm.read_inode(target)?.mode == oifs::inode::FileType::Directory;
            if is_dir && !*recursive {
                // Guard against silently discarding a non-empty directory.
                let entries = dm.list_dir(target)?;
                if !entries.is_empty() {
                    return Err(format!(
                        "Directory '{}' is not empty; pass --recursive to delete it and its contents.",
                        path
                    )
                    .into());
                }
            }
            if is_dir {
                const MAX_CLI_TRAVERSAL_DEPTH: usize = 256;
                fn remove_dir_contents_recursive(
                    dm: &OifsSession,
                    dir_inode: u64,
                    dir_path: &str,
                    depth: usize,
                    visited: &mut oifs::FxHashSet<u64>,
                ) -> Result<(), Box<dyn std::error::Error>> {
                    if depth >= MAX_CLI_TRAVERSAL_DEPTH {
                        return Err(format!(
                            "Directory nesting limit ({MAX_CLI_TRAVERSAL_DEPTH}) exceeded at '{dir_path}'"
                        )
                        .into());
                    }
                    if !visited.insert(dir_inode) {
                        return Err(format!(
                            "Directory cycle detected at '{dir_path}' (inode {dir_inode})"
                        )
                        .into());
                    }
                    for entry in dm.list_dir(dir_inode)? {
                        let child_path = format!("{dir_path}/{}", entry.name);
                        let child_inode = dm.read_inode(entry.inode)?;
                        if child_inode.mode == oifs::inode::FileType::Directory {
                            remove_dir_contents_recursive(
                                dm,
                                entry.inode,
                                &child_path,
                                depth + 1,
                                visited,
                            )?;
                        }
                        dm.delete_file(dir_inode, &entry.name)
                            .map_err(|e| format!("Failed to delete '{child_path}': {e}"))?;
                    }
                    visited.remove(&dir_inode);
                    Ok(())
                }

                let mut visited = oifs::FxHashSet::default();
                remove_dir_contents_recursive(&dm, target, path, 0, &mut visited)?;
            }

            dm.delete_file(parent_id, &filename)?;
            dm.flush()?;
            if cli.json {
                println!(
                    "{}",
                    json!({"ok": true, "removed": path, "inode": target, "recursive": recursive})
                );
            } else {
                println!("Removed '{}'", path);
            }
            Ok(())
        }
        Commands::Ls { path, recursive } => {
            if !image.exists() {
                return Err(format!("Image {:?} does not exist.", image).into());
            }
            let dm = open_session(
                image,
                &cli.password,
                &session_mode,
                cli.json,
                auto_defrag.as_ref(),
                cli.read_only,
            )?;

            let target_inode_id = if let Some(p) = path.as_ref() {
                dm.resolve_path(p)?
            } else {
                dm.superblock()?.root_inode
            };

            let target_inode = dm.read_inode(target_inode_id)?;

            let mut results = Vec::new();

            if target_inode.mode != FileType::Directory {
                let modified = format_local_timestamp(target_inode.modified_at);
                let comp_size = if target_inode.compressed_size > 0 {
                    Some(target_inode.compressed_size)
                } else {
                    None
                };
                results.push(LsEntry {
                    name: path.as_deref().unwrap_or(".").to_string(),
                    kind: "f".to_string(),
                    size: target_inode.size,
                    comp_size,
                    modified,
                });
            } else {
                const MAX_CLI_TRAVERSAL_DEPTH: usize = 256;
                fn collect_ls(
                    dm: &OifsSession,
                    inode_id: u64,
                    current_path: &str,
                    recursive: bool,
                    depth: usize,
                    visited: &mut oifs::FxHashSet<u64>,
                    results: &mut Vec<LsEntry>,
                ) -> Result<(), Box<dyn std::error::Error>> {
                    if depth >= MAX_CLI_TRAVERSAL_DEPTH || !visited.insert(inode_id) {
                        return Ok(());
                    }

                    let inode = dm.read_inode(inode_id)?;
                    if inode.mode != FileType::Directory {
                        return Ok(());
                    }

                    let entries = dm.list_dir(inode_id)?;
                    for dir_entry in entries {
                        let entry_inode = dm.read_inode(dir_entry.inode)?;
                        let full_path = if current_path.is_empty() || current_path == "." {
                            dir_entry.name.clone()
                        } else {
                            format!("{}/{}", current_path, dir_entry.name)
                        };

                        let modified = format_local_timestamp(entry_inode.modified_at);
                        let kind = if entry_inode.mode == FileType::Directory {
                            "d"
                        } else {
                            "f"
                        };
                        let comp_size = if entry_inode.compressed_size > 0 {
                            Some(entry_inode.compressed_size)
                        } else {
                            None
                        };

                        results.push(LsEntry {
                            name: full_path.clone(),
                            kind: kind.to_string(),
                            size: entry_inode.size,
                            comp_size,
                            modified,
                        });

                        if recursive && entry_inode.mode == FileType::Directory {
                            collect_ls(
                                dm,
                                dir_entry.inode,
                                &full_path,
                                true,
                                depth + 1,
                                visited,
                                results,
                            )?;
                        }
                    }
                    Ok(())
                }
                let base_path = path.as_deref().unwrap_or("");
                let mut visited = oifs::FxHashSet::default();
                collect_ls(
                    &dm,
                    target_inode_id,
                    base_path,
                    *recursive,
                    0,
                    &mut visited,
                    &mut results,
                )?;
            }

            if cli.json {
                println!("{}", serde_json::to_string(&results)?);
            } else {
                if target_inode.mode != FileType::Directory {
                    let r = &results[0];
                    println!("{:<20} {:<10} {:<25}", r.name, r.size, r.modified);
                } else {
                    println!(
                        "{:<40} {:<10} {:<10} {:<25}",
                        "Name", "Size", "CompSize", "Modified"
                    );
                    println!("{:-<40} {:-<10} {:-<10} {:-<25}", "", "", "", "");
                    for r in results {
                        let comp_str = r
                            .comp_size
                            .map(|s| s.to_string())
                            .unwrap_or("-".to_string());
                        let type_char = if r.kind == "d" { "d" } else { "-" };
                        println!(
                            "{} {:<38} {:<10} {:<10} {:<25}",
                            type_char, r.name, r.size, comp_str, r.modified
                        );
                    }
                }
            }
            Ok(())
        }
        Commands::Analyze => {
            if !image.exists() {
                return Err(format!("Image {:?} does not exist.", image).into());
            }
            let dm = open_session(
                image,
                &cli.password,
                &session_mode,
                cli.json,
                auto_defrag.as_ref(),
                cli.read_only,
            )?;
            let stats = dm.analyze_fragmentation()?;

            if cli.json {
                println!(
                    "{}",
                    json!({
                        "ok": true,
                        "total_blocks": stats.total_blocks,
                        "used_blocks": stats.used_blocks,
                        "free_blocks": stats.free_blocks,
                        "free_runs": stats.free_runs,
                        "largest_free_run": stats.largest_free_run,
                        "avg_gap_size": stats.avg_gap_size,
                        "fragmentation_ratio": stats.fragmentation_ratio
                    })
                );
            } else {
                println!("\n=== Disk Fragmentation Analysis ===");
                println!(
                    "Total blocks:        {} blocks ({} KB)",
                    stats.total_blocks,
                    stats.total_blocks * 4
                );
                println!(
                    "Used blocks:         {} blocks ({} KB)",
                    stats.used_blocks,
                    stats.used_blocks * 4
                );
                println!(
                    "Free blocks:         {} blocks ({} KB)",
                    stats.free_blocks,
                    stats.free_blocks * 4
                );
                println!();
                println!("Free runs (gaps):    {}", stats.free_runs);
                println!(
                    "Largest free run:    {} blocks ({} KB)",
                    stats.largest_free_run,
                    stats.largest_free_run * 4
                );
                println!("Avg gap size:        {:.2} blocks", stats.avg_gap_size);
                println!();
                println!(
                    "Fragmentation ratio: {:.2}%",
                    stats.fragmentation_ratio * 100.0
                );
            }
            Ok(())
        }
        Commands::Defrag { mode } => {
            if cli.read_only {
                return Err(oifs::disk::DiskManagerError::ReadOnly.into());
            }
            if !image.exists() {
                return Err(format!("Image {:?} does not exist.", image).into());
            }

            let defrag_mode = match mode.to_lowercase().as_str() {
                "safe" => DefragMode::Safe,
                "inplace" => DefragMode::InPlace,
                _ => {
                    return Err(format!("Invalid mode '{}'. Use 'safe' or 'inplace'.", mode).into());
                }
            };

            let image_path = image.to_str().ok_or("Invalid image path")?;

            if !cli.json {
                println!("Starting defragmentation in {:?} mode...", defrag_mode);
                if defrag_mode == DefragMode::InPlace {
                    println!("⚠️  WARNING: In-place mode directly modifies the image!");
                    println!("⚠️  Data loss may occur if interrupted. Press Ctrl+C to cancel.");
                    std::thread::sleep(std::time::Duration::from_secs(3));
                }
            }

            let dm = open_session(
                image,
                &cli.password,
                &session_mode,
                cli.json,
                auto_defrag.as_ref(),
                cli.read_only,
            )?;
            match dm.defragment(image_path, defrag_mode, None) {
                Ok(stats) => {
                    if cli.json {
                        println!(
                            "{}",
                            json!({
                                "ok": true,
                                "files_processed": stats.files_processed,
                                "bytes_moved": stats.bytes_moved,
                                "frag_before": stats.frag_before,
                                "frag_after": stats.frag_after
                            })
                        );
                    } else {
                        println!("\n✅ Defragmentation complete!");
                        println!("Files processed:     {}", stats.files_processed);
                        println!(
                            "Bytes moved:         {} bytes ({:.2} KB)",
                            stats.bytes_moved,
                            stats.bytes_moved as f64 / 1024.0
                        );
                        println!("Fragmentation:");
                        println!("  Before:            {:.2}%", stats.frag_before * 100.0);
                        println!("  After:             {:.2}%", stats.frag_after * 100.0);
                        println!(
                            "  Improvement:       {:.2}%",
                            (stats.frag_before - stats.frag_after) * 100.0
                        );
                    }
                }
                Err(e) => {
                    return Err(e.into());
                }
            }
            Ok(())
        }
        Commands::Migrate => {
            if cli.read_only {
                return Err(oifs::disk::DiskManagerError::ReadOnly.into());
            }
            if !image.exists() {
                return Err(format!("Image {:?} does not exist.", image).into());
            }
            let dm = open_session(
                image,
                &cli.password,
                &session_mode,
                cli.json,
                auto_defrag.as_ref(),
                cli.read_only,
            )?;

            if dm.migration_in_progress()? {
                let msg = "A previous migration was interrupted; resuming it.";
                if cli.json {
                    println!("{}", json!({"ok": true, "resumed": true, "message": msg}));
                } else {
                    println!("↻ {}", msg);
                }
            }

            let stats = dm.migrate()?;
            if cli.json {
                println!(
                    "{}",
                    json!({
                        "ok": true,
                        "from_version": stats.from_version,
                        "to_version": stats.to_version,
                        "inodes_rewritten": stats.inodes_rewritten,
                        "already_current": stats.already_current,
                    })
                );
            } else if stats.already_current {
                println!(
                    "✅ Already at inode format v{}; nothing to do.",
                    stats.to_version
                );
            } else {
                println!(
                    "✅ Migrated inode format v{} → v{} ({} inode(s) rewritten).",
                    stats.from_version, stats.to_version, stats.inodes_rewritten
                );
            }
            Ok(())
        }
        Commands::Fsck => {
            if !image.exists() {
                return Err(format!("Image {:?} does not exist.", image).into());
            }
            let dm = open_session(
                image,
                &cli.password,
                &session_mode,
                cli.json,
                auto_defrag.as_ref(),
                cli.read_only,
            )?;
            match dm.verify_integrity() {
                Ok(report) => {
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&report).unwrap());
                    } else {
                        println!("\n=== Filesystem Consistency Check (fsck) ===");
                        println!(
                            "Status:              {}",
                            if report.is_clean {
                                "✅ CLEAN"
                            } else {
                                "❌ CORRUPTED"
                            }
                        );
                        println!("Orphan Inodes:       {}", report.orphan_inodes.len());
                        if !report.orphan_inodes.is_empty() {
                            println!("  IDs: {:?}", report.orphan_inodes);
                        }
                        println!("Leaked Blocks:       {}", report.leaked_blocks.len());
                        if !report.leaked_blocks.is_empty() {
                            println!("  IDs: {:?}", report.leaked_blocks);
                        }
                        println!("Missing Blocks:      {}", report.missing_blocks.len());
                        if !report.missing_blocks.is_empty() {
                            println!("  IDs: {:?}", report.missing_blocks);
                        }
                        println!("Cross-Linked Blocks: {}", report.cross_linked_blocks.len());
                        if !report.cross_linked_blocks.is_empty() {
                            println!("  IDs: {:?}", report.cross_linked_blocks);
                        }
                        println!("===========================================");
                        if !report.is_clean {
                            return Err("Filesystem consistency check failed".into());
                        }
                    }
                }
                Err(e) => {
                    return Err(e.into());
                }
            }
            Ok(())
        }
        Commands::Truncate { path, size } => {
            if cli.read_only {
                return Err(oifs::disk::DiskManagerError::ReadOnly.into());
            }
            if !image.exists() {
                return Err(format!("Image {:?} does not exist.", image).into());
            }
            let dm = open_session(
                image,
                &cli.password,
                &session_mode,
                cli.json,
                auto_defrag.as_ref(),
                cli.read_only,
            )?;
            dm.truncate_path(path, *size)?;
            dm.flush()?;
            if cli.json {
                println!("{}", json!({"ok": true, "path": path, "size": size}));
            } else {
                println!("Truncated '{}' to {} bytes", path, size);
            }
            Ok(())
        }
        Commands::Mount {
            mountpoint,
            backend,
            port,
            foreground,
            allow_other,
        } => {
            if !image.exists() {
                return Err(format!("Image {:?} does not exist.", image).into());
            }
            let resolved_backend = match backend.as_str() {
                "auto" => {
                    #[cfg(target_os = "macos")]
                    {
                        "nfs"
                    }
                    #[cfg(not(target_os = "macos"))]
                    {
                        "fuse"
                    }
                }
                "nfs" => "nfs",
                "fuse" => "fuse",
                other => {
                    return Err(format!(
                        "Invalid --backend '{}': expected auto, nfs, or fuse",
                        other
                    )
                    .into());
                }
            };

            let session = open_session(
                image,
                &cli.password,
                &oifs::ipc::SessionMode::Standalone,
                cli.json,
                auto_defrag.as_ref(),
                cli.read_only,
            )?;
            let dm = match session {
                OifsSession::Direct { dm, .. } => dm,
                OifsSession::Remote { .. } => {
                    return Err("Mount requires direct local access to the OIFS image".into());
                }
            };

            let is_ro = cli.read_only || dm.is_read_only();

            if resolved_backend == "nfs" {
                let nfs_opts = oifs::nfs::NfsMountOptions {
                    read_only: is_ro,
                    port: *port,
                    ..oifs::nfs::NfsMountOptions::default()
                };
                let shutdown_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let shutdown_sig = std::sync::Arc::clone(&shutdown_flag);
                let mp_clone = mountpoint.clone();
                let dm_clone = std::sync::Arc::clone(&dm);
                let _interrupt_guard = set_interrupt_handler(move || {
                    shutdown_sig.store(true, std::sync::atomic::Ordering::Release);
                    let _ = oifs::fuse::unmount(&mp_clone);
                    if !is_ro {
                        let _ = dm_clone.flush();
                    }
                    std::process::exit(0);
                })?;

                if *foreground && !cli.json {
                    eprintln!(
                        "Mounting {:?} at {:?} via local NFSv3 (read_only={}, port={})...",
                        image, mountpoint, is_ro, port
                    );
                }

                oifs::nfs::mount_and_serve_nfs(
                    std::sync::Arc::clone(&dm),
                    mountpoint,
                    nfs_opts,
                    Some(shutdown_flag),
                )?;
                if !is_ro {
                    dm.flush()?;
                }
                if cli.json {
                    println!(
                        "{}",
                        json!({
                            "ok": true,
                            "backend": "nfs",
                            "mountpoint": mountpoint.to_string_lossy(),
                            "read_only": is_ro,
                            "foreground": foreground,
                        })
                    );
                }
                return Ok(());
            }

            let mut mount_opts = oifs::fuse::FuseMountOptions::for_host_kernel();
            mount_opts.read_only = is_ro;
            mount_opts.allow_other = *allow_other;
            if let Some(stem) = image.file_stem().and_then(|s| s.to_str())
                && !stem.is_empty()
                && !stem.contains(',')
            {
                mount_opts.volname = Some(stem.to_string());
            }

            let fuse_fs = oifs::fuse::OifsFuseFs::from_arc(std::sync::Arc::clone(&dm), mount_opts);

            let mp_clone = mountpoint.clone();
            let dm_clone = std::sync::Arc::clone(&dm);
            let _interrupt_guard = set_interrupt_handler(move || {
                let _ = oifs::fuse::unmount(&mp_clone);
                if !is_ro {
                    let _ = dm_clone.flush();
                }
                std::process::exit(0);
            })?;

            if *foreground && !cli.json {
                eprintln!(
                    "Mounting {:?} at {:?} via FUSE (read_only={}, allow_other={})...",
                    image, mountpoint, is_ro, allow_other
                );
            }

            fuse_fs.mount(mountpoint)?;
            if !is_ro {
                dm.flush()?;
            }
            if cli.json {
                println!(
                    "{}",
                    json!({
                        "ok": true,
                        "backend": "fuse",
                        "mountpoint": mountpoint.to_string_lossy(),
                        "read_only": is_ro,
                        "foreground": foreground,
                        "allow_other": allow_other,
                    })
                );
            }
            Ok(())
        }
        Commands::Transfer {
            to,
            src_path,
            dest,
            move_file,
            overwrite,
            to_password,
            offset,
            len,
            dst_offset,
            trim_mode,
        } => {
            if !image.exists() {
                return Err(format!("Source image {:?} does not exist.", image).into());
            }
            if !to.exists() {
                return Err(format!("Destination image {:?} does not exist.", to).into());
            }
            let parsed_trim = match trim_mode.as_str() {
                "keep" => oifs::SpliceTrimMode::Keep,
                "punch-hole" | "punch" => oifs::SpliceTrimMode::PunchHole,
                "collapse" | "collapse-range" => oifs::SpliceTrimMode::CollapseRange,
                other => {
                    return Err(format!(
                        "Invalid --trim-mode '{}': expected keep, punch-hole, or collapse",
                        other
                    )
                    .into());
                }
            };
            let is_splice = offset.is_some()
                || len.is_some()
                || dst_offset.is_some()
                || parsed_trim != oifs::SpliceTrimMode::Keep;

            if (*move_file || parsed_trim != oifs::SpliceTrimMode::Keep) && cli.read_only {
                return Err(oifs::DiskManagerError::ReadOnly.into());
            }
            let src_session = open_session(
                image,
                &cli.password,
                &session_mode,
                cli.json,
                None,
                cli.read_only,
            )?;
            let dst_session = open_session(to, to_password, &session_mode, cli.json, None, false)?;

            let split_parent_name =
                |session: &OifsSession,
                 raw_path: &str|
                 -> Result<(u64, String), Box<dyn std::error::Error>> {
                    let trimmed = raw_path.trim_matches('/');
                    if trimmed.is_empty() {
                        return Err("Path cannot be root '/'".into());
                    }
                    match trimmed.rsplit_once('/') {
                        Some((parent_str, name)) => {
                            let parent_id = session.resolve_path(parent_str)?;
                            Ok((parent_id, name.to_string()))
                        }
                        None => Ok((0, trimmed.to_string())),
                    }
                };

            let (src_parent, src_name) = split_parent_name(&src_session, src_path)?;
            let dst_path_str = dest.as_deref().unwrap_or(src_path.as_str());
            let (dst_parent, dst_name) = split_parent_name(&dst_session, dst_path_str)?;

            if is_splice {
                let src_ino = src_session.lookup(src_parent, &src_name)?;
                let src_inode = src_session.read_inode(src_ino)?;
                if src_inode.mode != FileType::File {
                    return Err("transfer only supports regular files".into());
                }
                let src_off = offset.unwrap_or(0);
                let dst_off = dst_offset.unwrap_or(src_off);
                let splice_len = len.unwrap_or_else(|| src_inode.size.saturating_sub(src_off));
                let dst_ino = match dst_session.lookup(dst_parent, &dst_name) {
                    Ok(id) => id,
                    Err(_) => dst_session.create_file(dst_parent, &dst_name)?,
                };

                let stats = match (&src_session, &dst_session) {
                    (
                        OifsSession::Direct { dm: src_dm, .. },
                        OifsSession::Direct { dm: dst_dm, .. },
                    ) => src_dm.splice_range_to(
                        src_ino,
                        src_off,
                        dst_dm,
                        dst_ino,
                        dst_off,
                        splice_len,
                        oifs::SpliceOptions {
                            trim_mode: parsed_trim,
                        },
                    )?,
                    _ => {
                        let eff_len = if src_off >= src_inode.size {
                            0
                        } else {
                            splice_len.min(src_inode.size - src_off)
                        };
                        let mut buf = vec![0u8; eff_len as usize];
                        let mut got = 0usize;
                        while got < buf.len() {
                            let r = src_session.read_at(
                                src_ino,
                                src_off + got as u64,
                                &mut buf[got..],
                            )?;
                            if r == 0 {
                                buf.truncate(got);
                                break;
                            }
                            got += r;
                        }
                        let dst_inode = dst_session.read_inode(dst_ino)?;
                        let dst_seekable =
                            (dst_inode.flags & oifs::inode::INODE_FLAG_SEEKABLE_64K) != 0;
                        let dst_mode = if dst_seekable {
                            CompressionMode::Seekable {
                                chunk_size: oifs::inode::CHUNK_SIZE_64K as u32,
                                level: 1,
                            }
                        } else if dst_inode.compressed_size > 0 {
                            CompressionMode::Always
                        } else {
                            CompressionMode::Never
                        };
                        let dst_filters = oifs::filters::FilterConfig {
                            typesize: dst_inode.filter_typesize,
                            delta: dst_inode.filter_delta,
                            shuffle: dst_inode.filter_shuffle,
                            bitshuffle: dst_inode.filter_bitshuffle,
                        };
                        if !buf.is_empty() {
                            dst_session.write_data_with_filters(
                                dst_ino,
                                dst_off,
                                &buf,
                                dst_mode,
                                dst_filters,
                            )?;
                        }
                        if parsed_trim != oifs::SpliceTrimMode::Keep && !buf.is_empty() {
                            dst_session.flush()?;
                            let src_seekable =
                                (src_inode.flags & oifs::inode::INODE_FLAG_SEEKABLE_64K) != 0;
                            let src_mode = if src_seekable {
                                CompressionMode::Seekable {
                                    chunk_size: oifs::inode::CHUNK_SIZE_64K as u32,
                                    level: 1,
                                }
                            } else if src_inode.compressed_size > 0 {
                                CompressionMode::Always
                            } else {
                                CompressionMode::Never
                            };
                            let src_filters = oifs::filters::FilterConfig {
                                typesize: src_inode.filter_typesize,
                                delta: src_inode.filter_delta,
                                shuffle: src_inode.filter_shuffle,
                                bitshuffle: src_inode.filter_bitshuffle,
                            };
                            match parsed_trim {
                                oifs::SpliceTrimMode::Keep => {}
                                oifs::SpliceTrimMode::PunchHole => {
                                    let zeros = vec![0u8; buf.len()];
                                    src_session.write_data_with_filters(
                                        src_ino,
                                        src_off,
                                        &zeros,
                                        src_mode,
                                        src_filters,
                                    )?;
                                }
                                oifs::SpliceTrimMode::CollapseRange => {
                                    let mut full = src_session.read_data(src_ino)?;
                                    let s = (src_off as usize).min(full.len());
                                    let e = ((src_off + buf.len() as u64) as usize).min(full.len());
                                    full.drain(s..e);
                                    src_session.write_data_with_filters(
                                        src_ino,
                                        0,
                                        &full,
                                        src_mode,
                                        src_filters,
                                    )?;
                                    src_session.truncate(src_ino, full.len() as u64)?;
                                }
                            }
                        }
                        oifs::SpliceStats {
                            src_inode: src_ino,
                            dst_inode: dst_ino,
                            bytes_spliced: buf.len() as u64,
                            logical_rmw_bytes: buf.len() as u64,
                            body_fast_path_bytes: 0,
                            physical_bytes: 0,
                            extents_transferred: 0,
                            src_blocks_freed: 0,
                            method: if buf.is_empty() {
                                oifs::TransferMethod::RawBlockCopy
                            } else {
                                oifs::TransferMethod::LogicalReencode
                            },
                        }
                    }
                };

                if cli.json {
                    println!(
                        "{}",
                        json!({
                            "ok": true,
                            "src_inode": stats.src_inode,
                            "dst_inode": stats.dst_inode,
                            "bytes_spliced": stats.bytes_spliced,
                            "logical_rmw_bytes": stats.logical_rmw_bytes,
                            "body_fast_path_bytes": stats.body_fast_path_bytes,
                            "physical_bytes": stats.physical_bytes,
                            "extents_transferred": stats.extents_transferred,
                            "src_blocks_freed": stats.src_blocks_freed,
                            "method": format!("{:?}", stats.method),
                            "trim_mode": trim_mode,
                        })
                    );
                } else {
                    println!(
                        "Spliced '{}' [{}..{}] -> '{}' @ {} ({} body bytes, {} RMW bytes, {} physical bytes, {} src blocks freed, {:?})",
                        src_path,
                        src_off,
                        src_off + stats.bytes_spliced,
                        dst_path_str,
                        dst_off,
                        stats.body_fast_path_bytes,
                        stats.logical_rmw_bytes,
                        stats.physical_bytes,
                        stats.src_blocks_freed,
                        stats.method,
                    );
                }
                return Ok(());
            }

            let stats = match (&src_session, &dst_session) {
                (
                    OifsSession::Direct { dm: src_dm, .. },
                    OifsSession::Direct { dm: dst_dm, .. },
                ) => src_dm.transfer_file_to(
                    src_parent,
                    &src_name,
                    dst_dm,
                    dst_parent,
                    &dst_name,
                    oifs::TransferOptions {
                        move_file: *move_file,
                        overwrite: *overwrite,
                    },
                )?,
                // If either side is a Remote IPC proxy, fall back to logical IPC read + write
                // so we never touch raw host FDs behind the Master process's back.
                _ => {
                    let src_ino = src_session.lookup(src_parent, &src_name)?;
                    let src_inode = src_session.read_inode(src_ino)?;
                    if src_inode.mode != FileType::File {
                        return Err("transfer only supports regular files".into());
                    }
                    let data = src_session.read_data(src_ino)?;

                    if let Ok(existing_ino) = dst_session.lookup(dst_parent, &dst_name) {
                        if !*overwrite {
                            return Err(format!("Destination '{dst_name}' already exists").into());
                        }
                        let existing = dst_session.read_inode(existing_ino)?;
                        if existing.mode == FileType::Directory {
                            return Err("Is a directory".into());
                        }
                        dst_session.delete_file(dst_parent, &dst_name)?;
                    }

                    let is_seekable = (src_inode.flags & oifs::inode::INODE_FLAG_SEEKABLE_64K) != 0;
                    let comp_mode = if is_seekable {
                        CompressionMode::Seekable {
                            chunk_size: oifs::inode::CHUNK_SIZE_64K as u32,
                            level: 3,
                        }
                    } else if src_inode.compressed_size > 0 {
                        CompressionMode::Always
                    } else {
                        CompressionMode::Never
                    };
                    let filter_cfg = oifs::filters::FilterConfig {
                        typesize: src_inode.filter_typesize,
                        delta: src_inode.filter_delta,
                        shuffle: src_inode.filter_shuffle,
                        bitshuffle: src_inode.filter_bitshuffle,
                    };

                    let dst_ino = dst_session.create_file(dst_parent, &dst_name)?;
                    if let Err(e) = dst_session
                        .write_data_with_filters(dst_ino, 0, &data, comp_mode, filter_cfg)
                    {
                        let _ = dst_session.delete_file(dst_parent, &dst_name);
                        return Err(e.into());
                    }
                    if *move_file {
                        dst_session.flush()?;
                        src_session.delete_file(src_parent, &src_name)?;
                    }
                    oifs::TransferStats {
                        src_inode: src_ino,
                        dst_inode: dst_ino,
                        logical_bytes: data.len() as u64,
                        physical_bytes: data.len() as u64,
                        extents_transferred: usize::from(!data.is_empty()),
                        method: oifs::TransferMethod::LogicalReencode,
                    }
                }
            };

            if cli.json {
                println!(
                    "{}",
                    json!({
                        "ok": true,
                        "src_inode": stats.src_inode,
                        "dst_inode": stats.dst_inode,
                        "logical_bytes": stats.logical_bytes,
                        "physical_bytes": stats.physical_bytes,
                        "extents_transferred": stats.extents_transferred,
                        "method": format!("{:?}", stats.method),
                        "moved": move_file,
                    })
                );
            } else {
                println!(
                    "{} '{}' -> '{}' ({} logical bytes, {} physical bytes, {} extents, {:?})",
                    if *move_file { "Moved" } else { "Copied" },
                    src_path,
                    dst_path_str,
                    stats.logical_bytes,
                    stats.physical_bytes,
                    stats.extents_transferred,
                    stats.method,
                );
            }
            Ok(())
        }
    }
}
