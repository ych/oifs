use clap::{Parser, Subcommand};
use oifs::disk::DiskManager;
use oifs::directory::DirectoryIterator;
use oifs::inode::FileType;
use std::path::PathBuf;
// use std::time::{Duration, UNIX_EPOCH};
use chrono::{DateTime, Local, TimeZone};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[arg(short, long)]
    image: PathBuf,

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
    },
    /// Import a file into the image
    Put {
        /// File path on host
        host_path: PathBuf,
        /// Destination filename in OIFS (optional, defaults to host filename)
        remote_name: Option<String>,
    },
    /// Export a file from the image
    Get {
        /// Filename in OIFS
        remote_name: String,
        /// Destination path on host (optional, defaults to remote filename)
        host_path: Option<PathBuf>,
    },
    /// Create a directory
    Mkdir {
        /// Directory name
        dir_name: String,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Create { size } => {
            if cli.image.exists() {
                eprintln!("Error: Image {:?} already exists.", cli.image);
                return Ok(());
            }
            let size_bytes = size * 1024 * 1024;
            let _dm = DiskManager::open(&cli.image, size_bytes)?;
            println!("Created image {:?} with size {}MB", cli.image, size);
            Ok(())
        }
        Commands::Put { host_path, remote_name } => {
            if !cli.image.exists() {
                eprintln!("Error: Image {:?} does not exist.", cli.image);
                return Ok(());
            }
            if !host_path.exists() {
                eprintln!("Error: Host file {:?} does not exist.", host_path);
                return Ok(());
            }

            // Decide on remote path
            let path_str = remote_name.clone().unwrap_or_else(|| {
                host_path.file_name().unwrap().to_string_lossy().to_string()
            });

            let dm = DiskManager::open(&cli.image, 0)?;
            
            // Set up Ctrl+C handler to flush
            let dm_clone = dm.clone();
            ctrlc::set_handler(move || {
                let _ = dm_clone.flush();
                std::process::exit(1);
            })?;

            // Resolve parent
            let (parent_id, filename) = match dm.resolve_parent(&path_str) {
                Ok(res) => res,
                Err(e) => {
                     // For root files, resolve_parent "file.txt" -> parts=["file.txt"] -> parent parts=[]
                     // My implementation handles this: parts=["file.txt"], parent_parts=[], loop skipped, returns (root, "file.txt")
                     // So we just check if error.
                     eprintln!("Error resolving parent for '{}': {}", path_str, e);
                     return Ok(());
                }
            };
            
            // Check if exist in parent
            if dm.lookup(parent_id, &filename).is_ok() {
                 eprintln!("Error: File '{}' already exists.", filename);
                 return Ok(());
            }
            
            let inode_id = dm.create_file(parent_id, &filename)?;
            
            // 2. Read host content
            let content = std::fs::read(host_path)?;
            
            // 3. Write data
            dm.write_data(inode_id, 0, &content)?;
            println!("Imported '{}' to image.", path_str);
            Ok(())
        }
        Commands::Get { remote_name, host_path } => {
             if !cli.image.exists() {
                eprintln!("Error: Image {:?} does not exist.", cli.image);
                return Ok(());
            }
            
            let dm = DiskManager::open(&cli.image, 0)?;
            // resolve path
            match dm.resolve_path(remote_name) {
                Ok(inode_id) => {
                    let data = dm.read_data(inode_id)?;
                    let dest = host_path.clone().unwrap_or_else(|| PathBuf::from(PathBuf::from(remote_name).file_name().unwrap()));
                    
                    // Create parent dirs for host if needed? Simple write for now.
                    if let Some(parent) = dest.parent() {
                         std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&dest, data)?;
                    println!("Exported '{}' to {:?}", remote_name, dest);
                }
                Err(_) => {
                    eprintln!("Error: File '{}' not found.", remote_name);
                }
            }
            Ok(())
        }
        Commands::Mkdir { dir_name } => {
            if !cli.image.exists() {
                eprintln!("Error: Image {:?} does not exist.", cli.image);
                return Ok(());
            }
            
            let dm = DiskManager::open(&cli.image, 0)?;
            
            let (parent_id, filename) = match dm.resolve_parent(dir_name) {
                Ok(res) => res,
                Err(e) => {
                    eprintln!("Error resolving parent: {}", e);
                    return Ok(());
                }
            };
            
            if dm.lookup(parent_id, &filename).is_ok() {
                 eprintln!("Error: Directory or file '{}' already exists.", dir_name);
                 return Ok(());
            }

            dm.create_directory(parent_id, &filename)?;
            println!("Created directory '{}'", dir_name);
            Ok(())
        }
        Commands::Ls { path, recursive } => {
             if !cli.image.exists() {
                eprintln!("Error: Image {:?} does not exist.", cli.image);
                return Ok(());
            }
            let dm = DiskManager::open(&cli.image, 0)?;
            
            let target_inode_id = if let Some(p) = path.as_ref() {
                match dm.resolve_path(p) {
                    Ok(id) => id,
                    Err(_) => {
                        eprintln!("Error: Path '{}' not found.", p);
                        return Ok(());
                    }
                }
            } else {
                dm.superblock().root_inode
            };

            let target_inode = dm.read_inode(target_inode_id)?;

            if target_inode.mode != FileType::Directory {
                let dt: DateTime<Local> = Local.timestamp_opt(target_inode.modified_at as i64, 0).unwrap();
                let time_str = dt.format("%Y-%m-%d %H:%M:%S").to_string();
                println!("{:<20} {:<10} {:<25}", path.as_deref().unwrap_or("."), target_inode.size, time_str);
                return Ok(());
            }

            println!("{:<40} {:<10} {:<10} {:<25}", "Name", "Size", "CompSize", "Modified");
            println!("{:-<40} {:-<10} {:-<10} {:-<25}", "", "", "", "");

            // Recursive helper
            fn dbg_ls(dm: &DiskManager, inode_id: u64, current_path: &str, recursive: bool) -> Result<(), Box<dyn std::error::Error>> {
                 let inode = dm.read_inode(inode_id)?;
                 if inode.mode != FileType::Directory { return Ok(()); }
                 
                 let dir_block_id = inode.blocks[0];
                 if dir_block_id == 0 { return Ok(()); }
                 
                 if let Some(block_data) = dm.get_block_copy(dir_block_id) {
                     let iter = DirectoryIterator::new(&block_data);
                     for entry in iter {
                         if let Ok(dir_entry) = entry {
                             let entry_inode = dm.read_inode(dir_entry.inode)?;
                             let full_path = if current_path.is_empty() {
                                 dir_entry.name.clone()
                             } else {
                                 if current_path == "." {
                                      dir_entry.name.clone()
                                 } else {
                                     format!("{}/{}", current_path, dir_entry.name)
                                 }
                             };
                             
                             let dt: DateTime<Local> = Local.timestamp_opt(entry_inode.modified_at as i64, 0).unwrap();
                             let time_str = dt.format("%Y-%m-%d %H:%M:%S").to_string();
                             let type_char = if entry_inode.mode == FileType::Directory { "d" } else { "-" };
                             let comp_str = if entry_inode.compressed_size > 0 { 
                                 format!("{}", entry_inode.compressed_size) 
                             } else { 
                                 "-".to_string() 
                             };
                             
                             println!("{} {:<38} {:<10} {:<10} {:<25}", type_char, full_path, entry_inode.size, comp_str, time_str);

                             if recursive && entry_inode.mode == FileType::Directory {
                                 dbg_ls(dm, dir_entry.inode, &full_path, true)?;
                             }
                         }
                     }
                 }
                 Ok(())
            }
            
            let base_path = path.as_deref().unwrap_or("");
            dbg_ls(&dm, target_inode_id, base_path, *recursive)?;

            Ok(())
        }
    }
}
