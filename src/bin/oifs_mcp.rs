//! OIFS MCP Server — Exposes the OIFS filesystem as MCP tools for AI Agents.
//!
//! Run: `cargo run --bin oifs_mcp`
//! Then configure your IDE (Cursor, Claude Desktop, etc.) to use it via stdio.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_router, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as TokioMutex;

use oifs::disk::{CompressionMode, DiskManager};
use oifs::directory::DirectoryIterator;
use oifs::inode::FileType;

// ── Default image config ──────────────────────────────────────────────
const DEFAULT_IMAGE: &str = "agent_memory.img";
const DEFAULT_SIZE_MB: u64 = 10;

// ── MCP Server struct ─────────────────────────────────────────────────
#[derive(Clone)]
struct OifsMcpServer {
    dm: Arc<TokioMutex<DiskManager>>,
    image_path: PathBuf,
    tool_router: ToolRouter<Self>,
}

impl OifsMcpServer {
    fn new(image_path: PathBuf) -> Result<Self> {
        let size_bytes = DEFAULT_SIZE_MB * 1024 * 1024;
        let dm = DiskManager::open(&image_path, size_bytes)?;
        Ok(Self {
            dm: Arc::new(TokioMutex::new(dm)),
            image_path,
            tool_router: Self::tool_router(),
        })
    }

    /// Helper: list entries under a directory inode.
    fn list_entries_inner(dm: &DiskManager, dir_inode_id: u64) -> Result<Vec<EntryInfo>> {
        let inode = dm.read_inode(dir_inode_id)?;
        if inode.mode != FileType::Directory {
            anyhow::bail!("inode {} is not a directory", dir_inode_id);
        }

        let block_id = inode.blocks[0];
        if block_id == 0 {
            return Ok(vec![]);
        }

        let block_data = dm
            .get_block_copy(block_id)
            .ok_or_else(|| anyhow::anyhow!("Failed to read directory block"))?;

        let mut entries = Vec::new();
        for entry_result in DirectoryIterator::new(&block_data) {
            let entry = entry_result?;
            let child_inode = dm.read_inode(entry.inode)?;
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

// ── Tool implementations ──────────────────────────────────────────────

#[tool_router]
impl OifsMcpServer {
    #[tool(description = "Write (create or overwrite) a file inside the OIFS sandbox image. Parent directories must already exist.")]
    async fn write_file(&self, Parameters(params): Parameters<WriteFileParams>) -> String {
        let dm = self.dm.lock().await;
        let result = (|| -> Result<String> {
            let (parent_id, name) = dm.resolve_parent(&params.path)?;
            let inode_id = match dm.lookup(parent_id, &name) {
                Ok(existing_id) => existing_id,
                Err(_) => dm.create_file(parent_id, &name)?,
            };
            dm.write_data(inode_id, 0, params.content.as_bytes(), CompressionMode::Auto)?;
            Ok(format!("{{\"ok\":true,\"inode\":{},\"bytes\":{}}}", inode_id, params.content.len()))
        })();
        match result {
            Ok(msg) => msg,
            Err(e) => format!("{{\"ok\":false,\"error\":\"{}\"}}", e),
        }
    }

    #[tool(description = "Read the contents of a file inside the OIFS sandbox image. Returns UTF-8 text content.")]
    async fn read_file(&self, Parameters(params): Parameters<ReadFileParams>) -> String {
        let dm = self.dm.lock().await;
        let result = (|| -> Result<String> {
            let inode_id = dm.resolve_path(&params.path)?;
            let data = dm.read_data(inode_id)?;
            String::from_utf8(data).map_err(|_| anyhow::anyhow!("File contains non-UTF-8 data"))
        })();
        match result {
            Ok(content) => content,
            Err(e) => format!("{{\"ok\":false,\"error\":\"{}\"}}", e),
        }
    }

    #[tool(description = "List files and directories at a given path inside the OIFS sandbox image. Returns one JSON object per line (JSONL).")]
    async fn list_dir(&self, Parameters(params): Parameters<ListDirParams>) -> String {
        let dm = self.dm.lock().await;
        let result = (|| -> Result<String> {
            let dir_inode_id = dm.resolve_path(&params.path)?;
            let entries = Self::list_entries_inner(&dm, dir_inode_id)?;
            if entries.is_empty() {
                return Ok("{\"entries\":0}".to_string());
            }
            let lines: Vec<String> = entries.iter().map(|e| {
                format!("{{\"name\":\"{}\",\"kind\":\"{}\",\"size\":{}}}", e.name, e.kind, e.size)
            }).collect();
            Ok(lines.join("\n"))
        })();
        match result {
            Ok(output) => output,
            Err(e) => format!("{{\"ok\":false,\"error\":\"{}\"}}", e),
        }
    }

    #[tool(description = "Create a directory inside the OIFS sandbox image. Parent directories must exist.")]
    async fn mkdir(&self, Parameters(params): Parameters<MkdirParams>) -> String {
        let dm = self.dm.lock().await;
        let result = (|| -> Result<String> {
            let (parent_id, name) = dm.resolve_parent(&params.path)?;
            let dir_id = dm.create_directory(parent_id, &name)?;
            Ok(format!("{{\"ok\":true,\"inode\":{}}}", dir_id))
        })();
        match result {
            Ok(msg) => msg,
            Err(e) => format!("{{\"ok\":false,\"error\":\"{}\"}}", e),
        }
    }

    #[tool(description = "Delete a file from the OIFS sandbox image.")]
    async fn delete_file(&self, Parameters(params): Parameters<DeleteFileParams>) -> String {
        let dm = self.dm.lock().await;
        let result = (|| -> Result<String> {
            let (parent_id, name) = dm.resolve_parent(&params.path)?;
            dm.delete_file(parent_id, &name)?;
            Ok("{\"ok\":true}".to_string())
        })();
        match result {
            Ok(msg) => msg,
            Err(e) => format!("{{\"ok\":false,\"error\":\"{}\"}}", e),
        }
    }

    #[tool(description = "Append a line to a file inside the OIFS sandbox image. Creates the file if it does not exist. Ideal for JSONL memory logs.")]
    async fn append_file(&self, Parameters(params): Parameters<AppendFileParams>) -> String {
        let dm = self.dm.lock().await;
        let result = (|| -> Result<String> {
            let (parent_id, name) = dm.resolve_parent(&params.path)?;
            let inode_id = match dm.lookup(parent_id, &name) {
                Ok(existing_id) => existing_id,
                Err(_) => dm.create_file(parent_id, &name)?,
            };
            let existing = dm.read_data(inode_id).unwrap_or_default();
            let mut new_content = existing;
            new_content.extend_from_slice(params.content.as_bytes());
            if !params.content.ends_with('\n') {
                new_content.push(b'\n');
            }
            dm.write_data(inode_id, 0, &new_content, CompressionMode::Never)?;
            Ok(format!("{{\"ok\":true,\"inode\":{},\"total_bytes\":{}}}", inode_id, new_content.len()))
        })();
        match result {
            Ok(msg) => msg,
            Err(e) => format!("{{\"ok\":false,\"error\":\"{}\"}}", e),
        }
    }

    #[tool(description = "Show filesystem status: image path, total/used/free blocks, and fragmentation ratio.")]
    async fn status(&self) -> String {
        let dm = self.dm.lock().await;
        let result = (|| -> Result<String> {
            let frag = dm.analyze_fragmentation()?;
            Ok(format!(
                "{{\"image\":\"{}\",\"total_blocks\":{},\"used_blocks\":{},\"free_blocks\":{},\"fragmentation\":{:.3}}}",
                self.image_path.display(), frag.total_blocks, frag.used_blocks, frag.free_blocks, frag.fragmentation_ratio
            ))
        })();
        match result {
            Ok(msg) => msg,
            Err(e) => format!("{{\"ok\":false,\"error\":\"{}\"}}", e),
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

#[tokio::main]
async fn main() -> Result<()> {
    let image_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_IMAGE));

    eprintln!("[oifs-mcp] Starting MCP server with image: {}", image_path.display());

    let server = OifsMcpServer::new(image_path)?;
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;

    Ok(())
}
