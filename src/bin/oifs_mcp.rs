//! OIFS MCP Server — Exposes the OIFS filesystem as MCP tools for AI Agents.
//!
//! Run: `cargo run --bin oifs_mcp`
//! Then configure your IDE (Cursor, Claude Desktop, etc.) to use it via stdio.

use std::path::PathBuf;
use std::sync::Arc;

type Result<T, E = Box<dyn std::error::Error + Send + Sync>> = std::result::Result<T, E>;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, ServiceExt, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use oifs::EncryptionCipher;
use oifs::disk::{
    AutoGrowConfig, CompressionMode, DiskManager, DiskManagerError, ResizeMethod, VacuumOptions,
};
use oifs::inode::FileType;
use oifs::storage::StorageMode;

// ── Default image config ──────────────────────────────────────────────
const DEFAULT_IMAGE: &str = "agent_memory.img";
const DEFAULT_SIZE_MB: u64 = 10;

// ── MCP Server struct & config ─────────────────────────────────────────
struct OifsMcpConfig {
    image_path: PathBuf,
    size_bytes: u64,
    password: Option<String>,
    journal: bool,
    storage_mode: StorageMode,
    read_only: bool,
    auto_grow: AutoGrowConfig,
    dict_path: Option<PathBuf>,
    cipher: EncryptionCipher,
}

#[derive(Clone)]
struct OifsMcpServer {
    dm: DiskManager,
    append_lock: Arc<std::sync::Mutex<()>>,
    image_path: PathBuf,
    tool_router: ToolRouter<Self>,
}

impl OifsMcpServer {
    fn new(cfg: OifsMcpConfig) -> Result<Self> {
        if cfg.read_only && cfg.auto_grow.enabled {
            return Err("Cannot combine --read-only with --auto-grow".into());
        }
        let password = cfg.password.as_deref();
        let dm = if cfg.read_only {
            DiskManager::open_read_only_with_storage_mode(
                &cfg.image_path,
                password,
                cfg.storage_mode,
            )?
        } else {
            DiskManager::open_with_options_and_cipher(
                &cfg.image_path,
                cfg.size_bytes,
                password,
                cfg.journal,
                cfg.storage_mode,
                cfg.cipher,
            )?
        };
        if cfg.auto_grow.enabled {
            dm.set_auto_grow(cfg.auto_grow);
        }
        if let Some(dpath) = cfg.dict_path.as_ref() {
            let dict_bytes = std::fs::read(dpath)?;
            if cfg.read_only {
                dm.register_zstd_dict(&dict_bytes)?;
            } else {
                dm.register_and_use_zstd_dict(&dict_bytes)?;
            }
        }
        Ok(Self {
            dm,
            append_lock: Arc::new(std::sync::Mutex::new(())),
            image_path: cfg.image_path,
            tool_router: Self::tool_router(),
        })
    }

    /// Helper: list entries under a directory inode.
    fn list_entries_inner(dm: &DiskManager, dir_inode_id: u64) -> Result<Vec<EntryInfo>> {
        let sess = dm.begin_read();
        let inode = sess.read_inode(dir_inode_id)?;
        if inode.mode != FileType::Directory {
            return Err(format!("inode {} is not a directory", dir_inode_id).into());
        }

        let dir_entries = sess.list_dir(dir_inode_id)?;
        let mut entries = Vec::with_capacity(dir_entries.len());
        for entry in dir_entries {
            let child_inode = sess.read_inode(entry.inode)?;
            let kind = match child_inode.mode {
                FileType::File => "file",
                FileType::Directory => "dir",
            };
            entries.push(EntryInfo {
                name: entry.name,
                kind: kind.to_string(),
                size: child_inode.size,
            });
        }
        Ok(entries)
    }
}

// ── Parameter schemas ─────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct WriteFileParams {
    /// Path inside the OIFS image (e.g. "notes/todo.txt")
    path: String,
    /// UTF-8 content to write
    content: String,
}

#[derive(Deserialize, JsonSchema)]
struct ReadFileParams {
    /// Path inside the OIFS image (e.g. "notes/todo.txt")
    path: String,
}

#[derive(Deserialize, JsonSchema)]
struct ListDirParams {
    /// Path inside the OIFS image (e.g. "." for root, "notes" for a subdirectory)
    path: String,
}

#[derive(Deserialize, JsonSchema)]
struct MkdirParams {
    /// Path of the new directory (e.g. "notes/drafts")
    path: String,
}

#[derive(Deserialize, JsonSchema)]
struct DeleteFileParams {
    /// Path of the file to delete (e.g. "notes/old.txt")
    path: String,
}

#[derive(Deserialize, JsonSchema)]
struct TruncateFileParams {
    /// Path inside the OIFS image (e.g. "notes/todo.txt")
    path: String,
    /// Target file size in bytes
    size: u64,
}

#[derive(Deserialize, JsonSchema)]
struct AppendFileParams {
    /// Path inside the OIFS image (e.g. "logs/memory.jsonl")
    path: String,
    /// UTF-8 content to append (a newline is auto-added if missing)
    content: String,
}

#[derive(Deserialize, JsonSchema)]
struct RenamePathParams {
    /// Current path inside the OIFS image
    old_path: String,
    /// New path inside the OIFS image (parent directory must exist)
    new_path: String,
}

#[derive(Deserialize, JsonSchema)]
struct StatPathParams {
    /// Path inside the OIFS image (e.g. "." or "notes/todo.txt")
    path: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
enum McpBatchOp {
    /// Atomically create a new file with UTF-8 content
    CreateFile {
        path: String,
        content: String,
        /// Optional compression mode: "auto", "never", "always", or "seekable"
        #[serde(default)]
        compression: Option<String>,
    },
    /// Create a directory (parent must exist)
    Mkdir { path: String },
    /// Delete a file or empty directory
    Delete { path: String },
    /// Atomically rename or move an entry
    Rename { old_path: String, new_path: String },
}

#[derive(Deserialize, JsonSchema)]
struct WriteBatchParams {
    /// Ordered list of operations to commit atomically
    ops: Vec<McpBatchOp>,
}

#[derive(Deserialize, JsonSchema)]
struct ResizeImageParams {
    /// Target container size in bytes (rounded up to 4 KiB blocks)
    size_bytes: u64,
    /// If true, only allow growing (fail instead of shrinking)
    #[serde(default)]
    grow_only: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct VacuumImageParams {
    /// Minimum free headroom preserved after shrinking (bytes, default 1 MiB)
    #[serde(default)]
    min_headroom_bytes: Option<u64>,
    /// Percentage of used data blocks preserved as extra headroom (0..=100, default 15)
    #[serde(default)]
    headroom_percent: Option<u8>,
    /// Floor on post-vacuum image byte size (default 0)
    #[serde(default)]
    min_image_bytes: Option<u64>,
    /// Allow cross-boundary atomic re-layout when reclaiming space (default true)
    #[serde(default)]
    allow_relayout: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct ConfigureAutoGrowParams {
    /// Enable or disable transparent auto-grow on capacity exhaustion
    enabled: bool,
    /// Multiplicative capacity target (1.1..=4.0, default 1.5)
    #[serde(default)]
    growth_factor: Option<f64>,
    /// Minimum byte increment per expansion (default 4 MiB)
    #[serde(default)]
    min_increment_bytes: Option<u64>,
    /// Maximum container size cap in bytes (0 = unlimited)
    #[serde(default)]
    max_size_bytes: Option<u64>,
    /// Allow cross-boundary atomic re-layout (default true)
    #[serde(default)]
    allow_relayout: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct TrainZstdDictParams {
    /// Directory path inside the OIFS image containing sample files
    dir_path: String,
    /// Optional path inside the OIFS image to persist the trained raw dictionary
    #[serde(default)]
    save_path: Option<String>,
    /// Target maximum dictionary size in bytes (256..=16 MiB, default 65536)
    #[serde(default)]
    max_dict_bytes: Option<usize>,
    /// Maximum sample files to train from (default 1024)
    #[serde(default)]
    max_samples: Option<usize>,
    /// Also set as default compression dictionary on this mount (default true on RW)
    #[serde(default)]
    activate: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct LoadZstdDictParams {
    /// Path to a trained dictionary file stored inside the OIFS image
    path: String,
    /// Set as default compression dictionary (default true on RW)
    #[serde(default)]
    activate: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct SetDefaultZstdDictParams {
    /// Registered dictionary ID, or null/omitted to clear the default dictionary
    #[serde(default)]
    dict_id: Option<u32>,
}

#[derive(Serialize)]
struct EntryInfo {
    name: String,
    kind: String,
    size: u64,
}

// ── JSON helpers ───────────────────────────────────────────────────────
//
// The responses used to be assembled with `format!`, which does not
// escape quotes, backslashes or control characters. A file name or an error
// message could therefore inject arbitrary JSON into the response. Every
// response is now built with `serde_json`, which escapes correctly.

/// Builds a JSON object from key/value pairs.
fn json_object(fields: Vec<(&str, serde_json::Value)>) -> String {
    let mut map = serde_json::Map::new();
    for (key, value) in fields {
        map.insert(key.to_string(), value);
    }
    serde_json::Value::Object(map).to_string()
}

/// Builds the `{"ok":false,"error":...}` response of a failed tool call.
fn json_error(error: &dyn std::fmt::Display) -> String {
    json_object(vec![
        ("ok", serde_json::Value::Bool(false)),
        ("error", serde_json::Value::String(error.to_string())),
    ])
}

fn resize_method_name(method: ResizeMethod) -> &'static str {
    match method {
        ResizeMethod::NoOp => "no_op",
        ResizeMethod::InPlace => "in_place",
        ResizeMethod::AtomicRelayout => "atomic_relayout",
    }
}

fn parse_compression_str(s: Option<&str>) -> Result<CompressionMode> {
    match s {
        None => Ok(CompressionMode::Auto),
        Some(v) => match v.trim().to_ascii_lowercase().as_str() {
            "auto" | "default" => Ok(CompressionMode::Auto),
            "never" | "raw" | "none" => Ok(CompressionMode::Never),
            "always" | "stream" => Ok(CompressionMode::Always),
            "seekable" | "seekable_64k" => Ok(CompressionMode::Seekable {
                chunk_size: 65536,
                level: 3,
            }),
            other => Err(format!("Unsupported compression mode: '{other}'").into()),
        },
    }
}

fn auto_grow_json_fields(cfg: &AutoGrowConfig) -> Vec<(&'static str, serde_json::Value)> {
    vec![
        ("ok", serde_json::Value::Bool(true)),
        ("enabled", serde_json::Value::Bool(cfg.enabled)),
        ("growth_factor", serde_json::Value::from(cfg.growth_factor)),
        (
            "min_increment_bytes",
            serde_json::Value::from(cfg.min_increment_bytes),
        ),
        ("max_size_bytes", serde_json::Value::from(cfg.max_size_bytes)),
        ("allow_relayout", serde_json::Value::Bool(cfg.allow_relayout)),
    ]
}

// ── Tool implementations ──────────────────────────────────────────────

#[tool_router]
impl OifsMcpServer {
    #[tool(
        description = "Write (create or overwrite) a file inside the OIFS sandbox image. Parent directories must already exist."
    )]
    async fn write_file(&self, Parameters(params): Parameters<WriteFileParams>) -> String {
        let dm = self.dm.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<String> {
            let (parent_id, name) = dm.resolve_parent(&params.path)?;
            let inode_id = match dm.lookup(parent_id, &name) {
                Ok(existing_id) => existing_id,
                Err(_) => dm.create_file(parent_id, &name)?,
            };
            dm.write_data_truncated(inode_id, params.content.as_bytes(), CompressionMode::Auto)?;
            Ok(json_object(vec![
                ("ok", serde_json::Value::Bool(true)),
                ("inode", serde_json::Value::from(inode_id)),
                ("bytes", serde_json::Value::from(params.content.len())),
            ]))
        })
        .await;
        match result {
            Ok(Ok(msg)) => msg,
            Ok(Err(e)) => json_error(&e),
            Err(e) => json_error(&e),
        }
    }

    #[tool(
        description = "Read the contents of a file inside the OIFS sandbox image. Returns UTF-8 text content."
    )]
    async fn read_file(&self, Parameters(params): Parameters<ReadFileParams>) -> String {
        let dm = self.dm.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<String> {
            let inode_id = dm.resolve_path(&params.path)?;
            let data = dm.read_data(inode_id)?;
            String::from_utf8(data).map_err(|_| "File contains non-UTF-8 data".into())
        })
        .await;
        match result {
            Ok(Ok(content)) => content,
            Ok(Err(e)) => json_error(&e),
            Err(e) => json_error(&e),
        }
    }

    #[tool(
        description = "List files and directories at a given path inside the OIFS sandbox image. Returns one JSON object per line (JSONL)."
    )]
    async fn list_dir(&self, Parameters(params): Parameters<ListDirParams>) -> String {
        let dm = self.dm.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<String> {
            let dir_inode_id = dm.resolve_path(&params.path)?;
            let entries = Self::list_entries_inner(&dm, dir_inode_id)?;
            if entries.is_empty() {
                return Ok("{\"entries\":0}".to_string());
            }
            let lines: Vec<String> = entries
                .iter()
                .map(|e| {
                    json_object(vec![
                        ("name", serde_json::Value::String(e.name.clone())),
                        ("kind", serde_json::Value::String(e.kind.clone())),
                        ("size", serde_json::Value::from(e.size)),
                    ])
                })
                .collect();
            Ok(lines.join("\n"))
        })
        .await;
        match result {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => json_error(&e),
            Err(e) => json_error(&e),
        }
    }

    #[tool(
        description = "Create a directory inside the OIFS sandbox image. Parent directories must exist."
    )]
    async fn mkdir(&self, Parameters(params): Parameters<MkdirParams>) -> String {
        let dm = self.dm.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<String> {
            let (parent_id, name) = dm.resolve_parent(&params.path)?;
            let dir_id = dm.create_directory(parent_id, &name)?;
            Ok(json_object(vec![
                ("ok", serde_json::Value::Bool(true)),
                ("inode", serde_json::Value::from(dir_id)),
            ]))
        })
        .await;
        match result {
            Ok(Ok(msg)) => msg,
            Ok(Err(e)) => json_error(&e),
            Err(e) => json_error(&e),
        }
    }

    #[tool(description = "Delete a file from the OIFS sandbox image.")]
    async fn delete_file(&self, Parameters(params): Parameters<DeleteFileParams>) -> String {
        let dm = self.dm.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<String> {
            let (parent_id, name) = dm.resolve_parent(&params.path)?;
            dm.delete_file(parent_id, &name)?;
            Ok(json_object(vec![("ok", serde_json::Value::Bool(true))]))
        })
        .await;
        match result {
            Ok(Ok(msg)) => msg,
            Ok(Err(e)) => json_error(&e),
            Err(e) => json_error(&e),
        }
    }

    #[tool(
        description = "Truncate or extend a file in the OIFS sandbox image to the specified size in bytes."
    )]
    async fn truncate_file(&self, Parameters(params): Parameters<TruncateFileParams>) -> String {
        let dm = self.dm.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<String> {
            dm.truncate_path(&params.path, params.size)?;
            Ok(json_object(vec![
                ("ok", serde_json::Value::Bool(true)),
                ("size", serde_json::Value::from(params.size)),
            ]))
        })
        .await;
        match result {
            Ok(Ok(msg)) => msg,
            Ok(Err(e)) => json_error(&e),
            Err(e) => json_error(&e),
        }
    }

    #[tool(
        description = "Append a line to a file inside the OIFS sandbox image. Creates the file if it does not exist. Ideal for JSONL memory logs."
    )]
    async fn append_file(&self, Parameters(params): Parameters<AppendFileParams>) -> String {
        let dm = self.dm.clone();
        let append_lock = Arc::clone(&self.append_lock);
        let result = tokio::task::spawn_blocking(move || -> Result<String> {
            let _guard = append_lock.lock().unwrap_or_else(|e| e.into_inner());
            let (parent_id, name) = dm.resolve_parent(&params.path)?;
            let (inode_id, existing_size) = match dm.lookup(parent_id, &name) {
                Ok(existing_id) => {
                    let ino = dm.read_inode(existing_id)?;
                    (existing_id, ino.size)
                }
                Err(_) => (dm.create_file(parent_id, &name)?, 0),
            };
            let mut payload = params.content;
            if !payload.ends_with('\n') {
                payload.push('\n');
            }
            dm.write_data(
                inode_id,
                existing_size,
                payload.as_bytes(),
                CompressionMode::Never,
            )?;
            let total_bytes = existing_size.saturating_add(payload.len() as u64);
            Ok(json_object(vec![
                ("ok", serde_json::Value::Bool(true)),
                ("inode", serde_json::Value::from(inode_id)),
                ("total_bytes", serde_json::Value::from(total_bytes)),
            ]))
        })
        .await;
        match result {
            Ok(Ok(msg)) => msg,
            Ok(Err(e)) => json_error(&e),
            Err(e) => json_error(&e),
        }
    }

    #[tool(
        description = "Show filesystem status: image path, byte size, read-only/auto-grow state, Zstd dictionaries, total/used/free blocks, and fragmentation ratio."
    )]
    async fn status(&self) -> String {
        let dm = self.dm.clone();
        let image_display = self.image_path.display().to_string();
        let result = tokio::task::spawn_blocking(move || -> Result<String> {
            let frag = dm.analyze_fragmentation()?;
            let ag = dm.auto_grow_config();
            let default_dict = match dm.default_zstd_dict_id() {
                Some(id) => serde_json::Value::from(id),
                None => serde_json::Value::Null,
            };
            let reg_dicts: Vec<serde_json::Value> = dm
                .registered_zstd_dict_ids()
                .into_iter()
                .map(serde_json::Value::from)
                .collect();
            let sb = dm.superblock();
            let cipher_val = match dm.encryption_cipher() {
                Some(c) => serde_json::Value::String(c.as_str().to_string()),
                None => serde_json::Value::Null,
            };
            Ok(json_object(vec![
                ("image", serde_json::Value::String(image_display)),
                ("image_size_bytes", serde_json::Value::from(dm.image_size_bytes())),
                ("read_only", serde_json::Value::Bool(dm.is_read_only())),
                ("encrypted", serde_json::Value::Bool(sb.encrypted)),
                ("encryption_version", serde_json::Value::from(sb.encryption_version)),
                ("cipher", cipher_val),
                ("auto_grow_enabled", serde_json::Value::Bool(ag.enabled)),
                ("default_zstd_dict_id", default_dict),
                ("registered_zstd_dicts", serde_json::Value::Array(reg_dicts)),
                ("total_blocks", serde_json::Value::from(frag.total_blocks)),
                ("used_blocks", serde_json::Value::from(frag.used_blocks)),
                ("free_blocks", serde_json::Value::from(frag.free_blocks)),
                (
                    "fragmentation",
                    serde_json::Value::from(frag.fragmentation_ratio),
                ),
            ]))
        })
        .await;
        match result {
            Ok(Ok(msg)) => msg,
            Ok(Err(e)) => json_error(&e),
            Err(e) => json_error(&e),
        }
    }

    #[tool(
        description = "Atomically rename or move a file or directory inside the OIFS sandbox image (POSIX overwrite semantics)."
    )]
    async fn rename_path(&self, Parameters(params): Parameters<RenamePathParams>) -> String {
        let dm = self.dm.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<String> {
            let (old_parent, old_name) = dm.resolve_parent(&params.old_path)?;
            let (new_parent, new_name) = dm.resolve_parent(&params.new_path)?;
            dm.rename(old_parent, &old_name, new_parent, &new_name)?;
            Ok(json_object(vec![
                ("ok", serde_json::Value::Bool(true)),
                ("old_path", serde_json::Value::String(params.old_path)),
                ("new_path", serde_json::Value::String(params.new_path)),
            ]))
        })
        .await;
        match result {
            Ok(Ok(msg)) => msg,
            Ok(Err(e)) => json_error(&e),
            Err(e) => json_error(&e),
        }
    }

    #[tool(description = "Inspect metadata of a file or directory inside the OIFS sandbox image.")]
    async fn stat_path(&self, Parameters(params): Parameters<StatPathParams>) -> String {
        let dm = self.dm.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<String> {
            let inode_id = dm.resolve_path(&params.path)?;
            let inode = dm.read_inode(inode_id)?;
            let kind = match inode.mode {
                FileType::File => "file",
                FileType::Directory => "dir",
            };
            Ok(json_object(vec![
                ("ok", serde_json::Value::Bool(true)),
                ("inode", serde_json::Value::from(inode_id)),
                ("kind", serde_json::Value::String(kind.to_string())),
                ("size", serde_json::Value::from(inode.size)),
                (
                    "compressed_size",
                    serde_json::Value::from(inode.compressed_size),
                ),
                ("created_at", serde_json::Value::from(inode.created_at)),
                ("modified_at", serde_json::Value::from(inode.modified_at)),
                ("encrypted", serde_json::Value::Bool(inode.encrypted)),
                (
                    "seekable_64k",
                    serde_json::Value::Bool(
                        (inode.flags & oifs::inode::INODE_FLAG_SEEKABLE_64K) != 0,
                    ),
                ),
            ]))
        })
        .await;
        match result {
            Ok(Ok(msg)) => msg,
            Ok(Err(e)) => json_error(&e),
            Err(e) => json_error(&e),
        }
    }

    #[tool(
        description = "Execute multiple create_file, mkdir, delete, and rename operations atomically via WriteBatch."
    )]
    async fn write_batch(&self, Parameters(params): Parameters<WriteBatchParams>) -> String {
        let dm = self.dm.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<String> {
            let mut wb = dm.begin_write_batch();
            for op in params.ops {
                match op {
                    McpBatchOp::CreateFile {
                        path,
                        content,
                        compression,
                    } => {
                        let mode = parse_compression_str(compression.as_deref())?;
                        let (parent, name) = dm.resolve_parent(&path)?;
                        wb.create_file_with_data(parent, name, content.into_bytes(), mode);
                    }
                    McpBatchOp::Mkdir { path } => {
                        let (parent, name) = dm.resolve_parent(&path)?;
                        wb.mkdir(parent, name);
                    }
                    McpBatchOp::Delete { path } => {
                        let (parent, name) = dm.resolve_parent(&path)?;
                        wb.delete(parent, name);
                    }
                    McpBatchOp::Rename { old_path, new_path } => {
                        let (old_parent, old_name) = dm.resolve_parent(&old_path)?;
                        let (new_parent, new_name) = dm.resolve_parent(&new_path)?;
                        wb.rename(old_parent, old_name, new_parent, new_name);
                    }
                }
            }
            let res = wb.commit()?;
            let created: Vec<serde_json::Value> = res
                .created_inodes
                .into_iter()
                .map(serde_json::Value::from)
                .collect();
            Ok(json_object(vec![
                ("ok", serde_json::Value::Bool(true)),
                ("ops_executed", serde_json::Value::from(res.ops_executed)),
                (
                    "wal_transactions",
                    serde_json::Value::from(res.wal_transactions),
                ),
                ("created_inodes", serde_json::Value::Array(created)),
            ]))
        })
        .await;
        match result {
            Ok(Ok(msg)) => msg,
            Ok(Err(e)) => json_error(&e),
            Err(e) => json_error(&e),
        }
    }

    #[tool(
        description = "Online grow or resize the OIFS container image to `size_bytes` (supports in-place and inode-preserving cross-boundary relayout)."
    )]
    async fn resize_image(&self, Parameters(params): Parameters<ResizeImageParams>) -> String {
        let dm = self.dm.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<String> {
            let stats = if params.grow_only.unwrap_or(false) {
                dm.grow(params.size_bytes)?
            } else {
                dm.resize(params.size_bytes)?
            };
            Ok(json_object(vec![
                ("ok", serde_json::Value::Bool(true)),
                ("bytes_before", serde_json::Value::from(stats.bytes_before)),
                ("bytes_after", serde_json::Value::from(stats.bytes_after)),
                ("blocks_before", serde_json::Value::from(stats.blocks_before)),
                ("blocks_after", serde_json::Value::from(stats.blocks_after)),
                ("inodes_before", serde_json::Value::from(stats.inodes_before)),
                ("inodes_after", serde_json::Value::from(stats.inodes_after)),
                (
                    "method",
                    serde_json::Value::String(resize_method_name(stats.method).to_string()),
                ),
            ]))
        })
        .await;
        match result {
            Ok(Ok(msg)) => msg,
            Ok(Err(e)) => json_error(&e),
            Err(e) => json_error(&e),
        }
    }

    #[tool(
        description = "Reclaim trailing free blocks and compact sparse space in the OIFS container image."
    )]
    async fn vacuum_image(&self, Parameters(params): Parameters<VacuumImageParams>) -> String {
        let dm = self.dm.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<String> {
            let mut opts = VacuumOptions::default();
            if let Some(bytes) = params.min_headroom_bytes {
                opts = opts.with_min_headroom_bytes(bytes);
            }
            if let Some(pct) = params.headroom_percent {
                opts = opts.with_headroom_percent(pct);
            }
            if let Some(floor) = params.min_image_bytes {
                opts = opts.with_min_image_bytes(floor);
            }
            if let Some(relayout) = params.allow_relayout {
                opts = opts.with_allow_relayout(relayout);
            }
            let stats = dm.vacuum(opts)?;
            Ok(json_object(vec![
                ("ok", serde_json::Value::Bool(true)),
                ("bytes_before", serde_json::Value::from(stats.bytes_before)),
                ("bytes_after", serde_json::Value::from(stats.bytes_after)),
                (
                    "bytes_reclaimed",
                    serde_json::Value::from(stats.bytes_reclaimed),
                ),
                ("blocks_before", serde_json::Value::from(stats.blocks_before)),
                ("blocks_after", serde_json::Value::from(stats.blocks_after)),
                (
                    "blocks_relocated",
                    serde_json::Value::from(stats.blocks_relocated),
                ),
                (
                    "method",
                    serde_json::Value::String(resize_method_name(stats.method).to_string()),
                ),
            ]))
        })
        .await;
        match result {
            Ok(Ok(msg)) => msg,
            Ok(Err(e)) => json_error(&e),
            Err(e) => json_error(&e),
        }
    }

    #[tool(
        description = "Configure dynamic Auto-Grow policy on the OIFS container (rejected on read-only mounts)."
    )]
    async fn configure_auto_grow(
        &self,
        Parameters(params): Parameters<ConfigureAutoGrowParams>,
    ) -> String {
        let dm = self.dm.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<String> {
            if dm.is_read_only() {
                return Err(DiskManagerError::ReadOnly.into());
            }
            let mut cfg = if params.enabled {
                AutoGrowConfig::enabled()
            } else {
                AutoGrowConfig::disabled()
            };
            if let Some(factor) = params.growth_factor {
                cfg = cfg.with_growth_factor(factor);
            }
            if let Some(min_inc) = params.min_increment_bytes {
                cfg = cfg.with_min_increment_bytes(min_inc);
            }
            if let Some(max_bytes) = params.max_size_bytes {
                cfg = cfg.with_max_size_bytes(max_bytes);
            }
            if let Some(relayout) = params.allow_relayout {
                cfg = cfg.with_allow_relayout(relayout);
            }
            dm.set_auto_grow(cfg);
            let effective = dm.auto_grow_config();
            Ok(json_object(auto_grow_json_fields(&effective)))
        })
        .await;
        match result {
            Ok(Ok(msg)) => msg,
            Ok(Err(e)) => json_error(&e),
            Err(e) => json_error(&e),
        }
    }

    #[tool(
        description = "Train a custom Zstd dictionary from sample files inside a directory in the OIFS image, optionally save it inside the image, and register/activate it."
    )]
    async fn train_zstd_dict(&self, Parameters(params): Parameters<TrainZstdDictParams>) -> String {
        let dm = self.dm.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<String> {
            if params.save_path.is_some() && dm.is_read_only() {
                return Err(DiskManagerError::ReadOnly.into());
            }
            let dir_inode = dm.resolve_path(&params.dir_path)?;
            let max_dict = params.max_dict_bytes.unwrap_or(64 * 1024);
            let max_samples = params.max_samples.unwrap_or(1024);
            let dict_bytes = dm.train_zstd_dict_from_dir(dir_inode, max_dict, max_samples)?;

            let activate = params.activate.unwrap_or(!dm.is_read_only());
            let dict_id = if activate {
                dm.register_and_use_zstd_dict(&dict_bytes)?
            } else {
                dm.register_zstd_dict(&dict_bytes)?
            };

            let saved_path_val = if let Some(save_path) = params.save_path {
                let (parent, name) = dm.resolve_parent(&save_path)?;
                match dm.lookup(parent, &name) {
                    Ok(existing_id) => {
                        dm.write_data_truncated(existing_id, &dict_bytes, CompressionMode::Never)?;
                    }
                    Err(_) => {
                        dm.create_file_with_data(
                            parent,
                            &name,
                            &dict_bytes,
                            CompressionMode::Never,
                        )?;
                    }
                }
                serde_json::Value::String(save_path)
            } else {
                serde_json::Value::Null
            };

            Ok(json_object(vec![
                ("ok", serde_json::Value::Bool(true)),
                ("dict_id", serde_json::Value::from(dict_id)),
                ("dict_bytes", serde_json::Value::from(dict_bytes.len())),
                ("activated", serde_json::Value::Bool(activate)),
                ("saved_path", saved_path_val),
            ]))
        })
        .await;
        match result {
            Ok(Ok(msg)) => msg,
            Ok(Err(e)) => json_error(&e),
            Err(e) => json_error(&e),
        }
    }

    #[tool(
        description = "Load and register a trained Zstd dictionary stored inside the OIFS image."
    )]
    async fn load_zstd_dict(&self, Parameters(params): Parameters<LoadZstdDictParams>) -> String {
        let dm = self.dm.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<String> {
            let inode_id = dm.resolve_path(&params.path)?;
            let dict_bytes = dm.read_data(inode_id)?;
            let activate = params.activate.unwrap_or(!dm.is_read_only());
            let dict_id = if activate {
                dm.register_and_use_zstd_dict(&dict_bytes)?
            } else {
                dm.register_zstd_dict(&dict_bytes)?
            };
            Ok(json_object(vec![
                ("ok", serde_json::Value::Bool(true)),
                ("dict_id", serde_json::Value::from(dict_id)),
                ("dict_bytes", serde_json::Value::from(dict_bytes.len())),
                ("activated", serde_json::Value::Bool(activate)),
            ]))
        })
        .await;
        match result {
            Ok(Ok(msg)) => msg,
            Ok(Err(e)) => json_error(&e),
            Err(e) => json_error(&e),
        }
    }

    #[tool(
        description = "Set or clear (omit/null) the active default Zstd compression dictionary ID on this mount."
    )]
    async fn set_default_zstd_dict(
        &self,
        Parameters(params): Parameters<SetDefaultZstdDictParams>,
    ) -> String {
        let dm = self.dm.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<String> {
            dm.set_default_zstd_dict(params.dict_id)?;
            let effective = match dm.default_zstd_dict_id() {
                Some(id) => serde_json::Value::from(id),
                None => serde_json::Value::Null,
            };
            Ok(json_object(vec![
                ("ok", serde_json::Value::Bool(true)),
                ("default_zstd_dict_id", effective),
            ]))
        })
        .await;
        match result {
            Ok(Ok(msg)) => msg,
            Ok(Err(e)) => json_error(&e),
            Err(e) => json_error(&e),
        }
    }

    #[tool(description = "Run online structural consistency check (fsck) on the OIFS image.")]
    async fn verify_integrity(&self) -> String {
        let dm = self.dm.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<String> {
            let report = dm.verify_integrity()?;
            Ok(json_object(vec![
                ("ok", serde_json::Value::Bool(true)),
                ("is_clean", serde_json::Value::Bool(report.is_clean)),
                (
                    "orphan_inodes",
                    serde_json::Value::from(report.orphan_inodes.len()),
                ),
                (
                    "leaked_blocks",
                    serde_json::Value::from(report.leaked_blocks.len()),
                ),
                (
                    "missing_blocks",
                    serde_json::Value::from(report.missing_blocks.len()),
                ),
                (
                    "cross_linked_blocks",
                    serde_json::Value::from(report.cross_linked_blocks.len()),
                ),
                (
                    "corrupted_inodes",
                    serde_json::Value::from(report.corrupted_inodes.len()),
                ),
            ]))
        })
        .await;
        match result {
            Ok(Ok(msg)) => msg,
            Ok(Err(e)) => json_error(&e),
            Err(e) => json_error(&e),
        }
    }
}

// ── ServerHandler ─────────────────────────────────────────────────────

#[rmcp::tool_handler]
impl ServerHandler for OifsMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "OIFS Memory Sandbox — A sandboxed inode filesystem for AI agent memory. \
                 All reads/writes are isolated inside a .img file."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────

fn print_usage() {
    eprintln!("Usage: oifs_mcp [OPTIONS] [IMAGE_PATH]");
    eprintln!();
    eprintln!("Options:");
    eprintln!(
        "  --size <MB>          Initial size when creating a new image (default: {} MB)",
        DEFAULT_SIZE_MB
    );
    eprintln!("  --journal            Enable metadata WAL journaling when creating a new image");
    eprintln!("  --no-mmap            Use pure positional pread/pwrite I/O (zero mmap)");
    eprintln!("  --read-only          Open existing image in strict read-only mode (F_RDLCK)");
    eprintln!(
        "  --auto-grow[=MAX_MB] Enable transparent auto-grow on capacity exhaustion (optional cap in MB)"
    );
    eprintln!("  --dict <PATH>        Load a trained Zstd dictionary file from host at startup");
    eprintln!("  --cipher <NAME>      Encryption cipher for new encrypted images: aes256-gcm (default) or xchacha20-poly1305");
    eprintln!("  -h, --help           Show this help");
    eprintln!();
    eprintln!("Environment:");
    eprintln!("  OIFS_PASSWORD        If set, opens (or creates) the image as encrypted.");
    eprintln!("                       Required to open an existing encrypted image.");
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut image_path: Option<PathBuf> = None;
    let mut size_mb: u64 = DEFAULT_SIZE_MB;
    let mut journal = false;
    let mut storage_mode = StorageMode::Mmap;
    let mut read_only = false;
    let mut auto_grow = AutoGrowConfig::disabled();
    let mut dict_path: Option<PathBuf> = None;
    let mut cipher = EncryptionCipher::default();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some(cap_str) = arg.strip_prefix("--auto-grow=") {
            let max_mb: u64 = cap_str
                .parse()
                .map_err(|e| format!("--auto-grow cap '{}' invalid: {}", cap_str, e))?;
            let max_bytes = max_mb
                .checked_mul(1024 * 1024)
                .ok_or_else(|| format!("--auto-grow cap is too large: {} MB", max_mb))?;
            auto_grow = AutoGrowConfig::enabled().with_max_size_bytes(max_bytes);
            continue;
        }
        match arg.as_str() {
            "--size" => {
                let v = args.next().ok_or("--size requires a value (MB)")?;
                size_mb = v
                    .parse()
                    .map_err(|e| format!("--size value '{}' invalid: {}", v, e))?;
            }
            "--journal" => journal = true,
            "--no-mmap" => storage_mode = StorageMode::FileIo,
            "--read-only" => read_only = true,
            "--auto-grow" => auto_grow = AutoGrowConfig::enabled(),
            "--dict" => {
                let v = args.next().ok_or("--dict requires a dictionary file path")?;
                dict_path = Some(PathBuf::from(v));
            }
            "--cipher" => {
                let v = args.next().ok_or("--cipher requires a cipher name")?;
                cipher = EncryptionCipher::parse(&v).map_err(|e| e.to_string())?;
            }
            "-h" | "--help" => {
                print_usage();
                return Ok(());
            }
            other if other.starts_with("--") => {
                return Err(format!("Unknown option: {}", other).into());
            }
            other if image_path.is_none() => {
                image_path = Some(PathBuf::from(other));
            }
            other => {
                return Err(format!("Unexpected positional argument: {}", other).into());
            }
        }
    }

    let image_path = image_path.unwrap_or_else(|| PathBuf::from(DEFAULT_IMAGE));
    // The multiplication used to overflow silently (a huge `--size`
    // wrapped around and produced a tiny image).
    let size_bytes = size_mb
        .checked_mul(1024)
        .and_then(|mb| mb.checked_mul(1024))
        .ok_or_else(|| format!("--size value is too large: {} MB", size_mb))?;
    let password = std::env::var("OIFS_PASSWORD").ok();

    eprintln!(
        "[oifs-mcp] image={} size={}MB encrypted={} cipher={} journal={} read_only={} auto_grow={}",
        image_path.display(),
        size_mb,
        password.is_some(),
        cipher,
        journal,
        read_only,
        auto_grow.enabled
    );

    let server = OifsMcpServer::new(OifsMcpConfig {
        image_path,
        size_bytes,
        password,
        journal,
        storage_mode,
        read_only,
        auto_grow,
        dict_path,
        cipher,
    })?;
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;

    Ok(())
}
