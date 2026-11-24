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

use oifs::disk::{CompressionMode, DiskManager};
use oifs::inode::FileType;

// ── Default image config ──────────────────────────────────────────────
const DEFAULT_IMAGE: &str = "agent_memory.img";
const DEFAULT_SIZE_MB: u64 = 10;

// ── MCP Server struct ─────────────────────────────────────────────────
#[derive(Clone)]
struct OifsMcpServer {
    dm: DiskManager,
    append_lock: Arc<std::sync::Mutex<()>>,
    image_path: PathBuf,
    tool_router: ToolRouter<Self>,
}

impl OifsMcpServer {
    fn new(image_path: PathBuf, size_bytes: u64, password: Option<&str>) -> Result<Self> {
        // If image doesn't yet exist AND a password was supplied,
        // bootstrap an encrypted image; otherwise open (auto-create plain).
        let dm = match (!image_path.exists(), password) {
            (true, Some(pwd)) => DiskManager::create_encrypted(&image_path, size_bytes, pwd)?,
            _ => DiskManager::open_with_password(&image_path, size_bytes, password)?,
        };
        Ok(Self {
            dm,
            append_lock: Arc::new(std::sync::Mutex::new(())),
            image_path,
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
        description = "Show filesystem status: image path, total/used/free blocks, and fragmentation ratio."
    )]
    async fn status(&self) -> String {
        let dm = self.dm.clone();
        let image_display = self.image_path.display().to_string();
        let result = tokio::task::spawn_blocking(move || -> Result<String> {
            let frag = dm.analyze_fragmentation()?;
            Ok(json_object(vec![
                ("image", serde_json::Value::String(image_display)),
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
        "  --size <MB>     Initial size when creating a new image (default: {} MB)",
        DEFAULT_SIZE_MB
    );
    eprintln!("  -h, --help      Show this help");
    eprintln!();
    eprintln!("Environment:");
    eprintln!("  OIFS_PASSWORD   If set, opens (or creates) the image as encrypted.");
    eprintln!("                  Required to open an existing encrypted image.");
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut image_path: Option<PathBuf> = None;
    let mut size_mb: u64 = DEFAULT_SIZE_MB;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--size" => {
                let v = args.next().ok_or("--size requires a value (MB)")?;
                size_mb = v
                    .parse()
                    .map_err(|e| format!("--size value '{}' invalid: {}", v, e))?;
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
        "[oifs-mcp] image={} size={}MB encrypted={}",
        image_path.display(),
        size_mb,
        password.is_some()
    );

    let server = OifsMcpServer::new(image_path, size_bytes, password.as_deref())?;
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;

    Ok(())
}
