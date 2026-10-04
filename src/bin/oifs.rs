use chrono::{DateTime, Local, TimeZone};
use clap::{Parser, Subcommand};
use oifs::disk::{CompressionMode, DefragMode};
use oifs::inode::FileType;
use oifs::session::OifsSession;
use serde::Serialize;
use serde_json::json;
use std::path::PathBuf;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[arg(short, long)]
    image: Option<PathBuf>,

    /// Password for encrypted filesystem (optional, will prompt if needed)
    #[arg(short, long)]
    password: Option<String>,

    /// Enable cross-machine / network mode (for NFS, Lustre, or multi-node clusters)
    #[arg(short = 'n', long = "network", global = true)]
    network: bool,

    /// Custom bind address for network mode (e.g. 0.0.0.0:9050 or 127.0.0.1:0)
    #[arg(long = "bind", global = true)]
    bind: Option<String>,

    /// Output results as minified JSON
    #[arg(long, global = true)]
    json: bool,

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
}

/// Helper function to read password securely from stdin (with terminal echo masked).
///
/// Prompt goes to stderr so stdout remains usable for JSON pipelines.
fn read_password(prompt: &str) -> Result<String, Box<dyn std::error::Error>> {
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
    Ok(password.trim().to_string())
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
) -> Result<OifsSession, Box<dyn std::error::Error>> {
    match OifsSession::open_with_mode(image_path, 0, mode.clone(), None, false) {
        Ok(s) => Ok(s),
        Err(oifs::session::SessionError::DiskManager(
            oifs::disk::DiskManagerError::PasswordRequired,
        )) => {
            let password = if let Some(pwd) = password_arg {
                pwd.clone()
            } else if let Ok(env_pwd) = std::env::var("OIFS_PASSWORD") {
                env_pwd
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

            OifsSession::open_with_mode(image_path, 0, mode.clone(), Some(&password), false)
                .map_err(|e| e.into())
        }
        Err(e) => Err(e.into()),
    }
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
    let session_mode = if cli.network || cli.bind.is_some() {
        oifs::ipc::SessionMode::Network {
            bind_addr: cli.bind.clone(),
        }
    } else {
        oifs::ipc::SessionMode::Local
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

    let image = cli
        .image
        .as_ref()
        .ok_or("Image path must be provided via -i / --image")?;

    match &cli.command {
        Commands::FilterAnalyze { .. } => unreachable!(),
        Commands::Create { size, encrypt } => {
            if image.exists() {
                return Err(format!("Image {:?} already exists.", image).into());
            }
            let size_bytes = size * 1024 * 1024;

            if *encrypt {
                let password = if let Some(pwd) = cli.password.clone() {
                    if pwd.is_empty() {
                        return Err("Password cannot be empty".into());
                    }
                    pwd
                } else if let Ok(env_pwd) = std::env::var("OIFS_PASSWORD") {
                    if env_pwd.is_empty() {
                        return Err("Password cannot be empty".into());
                    }
                    env_pwd
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

                let _session = OifsSession::open_with_mode(
                    image,
                    size_bytes,
                    session_mode,
                    Some(&password),
                    true,
                )?;
                if cli.json {
                    println!(
                        "{}",
                        json!({"ok": true, "message": "Encrypted filesystem created"})
                    );
                } else {
                    println!("✅ Encrypted filesystem created: {:?}", image);
                }
            } else {
                let _session =
                    OifsSession::open_with_mode(image, size_bytes, session_mode, None, false)?;
                if cli.json {
                    println!("{}", json!({"ok": true, "message": "Filesystem created"}));
                } else {
                    println!("Created image {:?} with size {}MB", image, size);
                }
            }
            Ok(())
        }
        Commands::Put {
            host_path,
            remote_name,
            compress,
            no_compress,
            filter,
            typesize,
        } => {
            if !image.exists() {
                return Err(format!("Image {:?} does not exist.", image).into());
            }
            if !host_path.exists() {
                return Err(format!("Host file {:?} does not exist.", host_path).into());
            }

            let path_str = remote_name
                .clone()
                .unwrap_or_else(|| host_path.file_name().unwrap().to_string_lossy().to_string());

            let compression_mode = if *compress {
                CompressionMode::Always
            } else if *no_compress {
                CompressionMode::Never
            } else {
                CompressionMode::Auto
            };

            let dm = open_session(image, &cli.password, &session_mode, cli.json)?;

            let dm_clone = dm.clone();
            ctrlc::set_handler(move || {
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
            if !image.exists() {
                return Err(format!("Image {:?} does not exist.", image).into());
            }
            let dm = open_session(image, &cli.password, &session_mode, cli.json)?;

            let dm_clone = dm.clone();
            ctrlc::set_handler(move || {
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
            let dm = open_session(image, &cli.password, &session_mode, cli.json)?;
            let inode_id = dm.resolve_path(remote_name)?;
            let data = dm.read_data(inode_id)?;
            let dest = host_path
                .clone()
                .unwrap_or_else(|| PathBuf::from(PathBuf::from(remote_name).file_name().unwrap()));

            if let Some(parent) = dest.parent() {
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
        Commands::Mkdir { dir_name } => {
            if !image.exists() {
                return Err(format!("Image {:?} does not exist.", image).into());
            }
            let dm = open_session(image, &cli.password, &session_mode, cli.json)?;
            let (parent_id, filename) = dm.resolve_parent(dir_name)?;

            if dm.lookup(parent_id, &filename).is_ok() {
                return Err(format!("Directory or file '{}' already exists.", dir_name).into());
            }

            let dir_id = dm.create_directory(parent_id, &filename)?;
            if cli.json {
                println!("{}", json!({"ok": true, "inode": dir_id}));
            } else {
                println!("Created directory '{}'", dir_name);
            }
            Ok(())
        }
        Commands::Ls { path, recursive } => {
            if !image.exists() {
                return Err(format!("Image {:?} does not exist.", image).into());
            }
            let dm = open_session(image, &cli.password, &session_mode, cli.json)?;

            let target_inode_id = if let Some(p) = path.as_ref() {
                dm.resolve_path(p)?
            } else {
                dm.superblock()?.root_inode
            };

            let target_inode = dm.read_inode(target_inode_id)?;

            let mut results = Vec::new();

            if target_inode.mode != FileType::Directory {
                let dt: DateTime<Local> = Local
                    .timestamp_opt(target_inode.modified_at as i64, 0)
                    .unwrap();
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
                    modified: dt.format("%Y-%m-%d %H:%M:%S").to_string(),
                });
            } else {
                fn collect_ls(
                    dm: &OifsSession,
                    inode_id: u64,
                    current_path: &str,
                    recursive: bool,
                    results: &mut Vec<LsEntry>,
                ) -> Result<(), Box<dyn std::error::Error>> {
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

                        let dt: DateTime<Local> = Local
                            .timestamp_opt(entry_inode.modified_at as i64, 0)
                            .unwrap();
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
                            modified: dt.format("%Y-%m-%d %H:%M:%S").to_string(),
                        });

                        if recursive && entry_inode.mode == FileType::Directory {
                            collect_ls(dm, dir_entry.inode, &full_path, true, results)?;
                        }
                    }
                    Ok(())
                }
                let base_path = path.as_deref().unwrap_or("");
                collect_ls(&dm, target_inode_id, base_path, *recursive, &mut results)?;
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
            let dm = open_session(image, &cli.password, &session_mode, cli.json)?;
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

            let dm = open_session(image, &cli.password, &session_mode, cli.json)?;
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
        Commands::Fsck => {
            if !image.exists() {
                return Err(format!("Image {:?} does not exist.", image).into());
            }
            let dm = open_session(image, &cli.password, &session_mode, cli.json)?;
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
    }
}
