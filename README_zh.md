# OIFS (O's Inode File System)

[English](README.md) | [繁體中文](README_zh.md) | [📖 線上官方文件](https://ych.github.io/oifs/)

> 🌐 **線上官方文件與架構視覺化網站**: [https://ych.github.io/oifs/](https://ych.github.io/oifs/)  
> 探索完整的互動式系統架構圖形導覽、模組呼叫關係、形式化數學證明與技術規格文件。

OIFS 是一個以 Rust（Edition 2024）打造的高效能、單檔嵌入式多行程 Inode 檔案系統引擎。具備斷電崩潰自癒 WAL、細粒度分片並行保護、強化級 AEAD 加密、科學數值前處理壓縮濾鏡、可隨機存取的 64 KiB 並行分塊引擎、可插拔異步 I/O 引擎、專為 AI 代理人設計的原生 Model Context Protocol (MCP) 伺服器，以及穩定的 C/C++ FFI 介面。

---

## 核心功能與系統架構 (Capabilities & Architecture)

### 1. 儲存與大容量擴展 (Storage & Scaling)
*   **類 Unix Inode 架構與 v2/v3 磁碟格式**：固定 256-byte 小端序 Inode 記錄（保留區歸零、無記憶體洩漏）與多區塊 v3 Bitmap（突破 128 MiB 映像檔上限），100% 向後相容舊版 v1 映像檔並支援 `oifs migrate` 原地升級。
*   **超大檔案支援（高達 513 GB）**：單級、雙級與三級間接區塊索引（134,480,394 個 4 KiB 區塊），具備批次間接指標葉節點解析與稀疏子樹跳洞加速。
*   **Seekable 64 KiB 並行分塊引擎**：獨立 64 KiB 壓縮／加密分塊，搭配零外部依賴的常駐 Scoped Worker Pool、全域回收 Zstd `CCtx`/`DCtx` 池、順序讀取 4-chunk 並行 Read-Ahead 預取、$O(\Delta \text{chunks})$ 漸進式截斷與間接表回收，以及非 Journal 模式的原地區塊重用。
*   **動態多區塊目錄 (Multi-Block Directory)**：目錄自動跨動態配置的 4 KiB Extent 區塊擴展，結合 64 位元 SipHash 檔名雜湊與 32 分片記憶體索引快取。
*   **線上與背景自動磁碟重組 (Online & Auto Defrag) 🧹**：支援 Safe 模式（原子 `.old` 備份置換）與 WAL 原子化 In-Place COW 重組，並提供可選的背景自動重組協調器（`--auto-defrag[=THRESHOLD]`）。
*   **FSCK 結構完整性診斷 🛠️**：完整一致性掃描器，偵測孤立 Inode、洩漏區塊、遺失區塊與交叉參照，支援文字與 JSON 輸出。

### 2. 事務性斷電崩潰自癒與持久化 (Transactional Crash Resilience & Durability)
*   **事務性 Metadata WAL (預寫日誌)**：可選啟用（`--journal`）的 CRC32C 校驗環狀預寫日誌，採 Ordered Mode（先落盤 Payload 再提交 Metadata）、冪等 Redo 重放、撕裂寫入偵測，掛載時自動復原。
*   **可設定持久化策略 (Configurable Durability Policies)**：
    *   `Strict`：合併單次系統呼叫同步 WAL Header+Frame，並採用 JBD2 式延遲原地 Metadata Checkpoint（**10.9k+ ops/sec**，較逐筆同步 metadata 快 **2.31 倍**）。
    *   `RangeAsync`：背景批次合併髒區間並於寫鎖外異步排空（**234k+ ops/sec**）。
    *   `Lazy`：共享 mmap / OS 頁快取寫回，達極限輸送量（**626k+ ops/sec**），可抗行程崩潰並於 `flush()` / `Drop` 落盤。
*   **非阻塞 Flush 並行保護**：`flush()` 與 `flush_async()` 僅持有共享讀鎖並透過專屬同步互斥鎖序列化 `msync`，絕不阻塞並行讀取執行緒。

### 3. 高並行多執行緒引擎 (High-Concurrency Multi-Threaded Engine)
*   **32 分片條帶化 Inode 與目錄快取**：`BoundedInodeCache`（Second-Chance Clock 淘汰）與 `ShardedDirCache`（鎖外建立完整目錄索引）皆以 64 位元費氏雜湊切分為 32 個獨立 `RwLock` 分片（16 執行緒下達 **8.60M+ lookups/sec**，鎖競爭下提速 **9.97 倍**）。
*   **鎖外處理管線 (Out-of-Lock Pipeline)**：濾鏡轉換、Zstd 壓縮與 XChaCha20-Poly1305 加密全程在檔案系統排他鎖之外執行，繁重多 MB 寫入下讀取零飢餓。
*   **零配置路徑解析**：零 Heap 配置的路徑分量迭代（`resolve_path_iter`）與 $O(1)$ 父目錄切分（`resolve_parent`），達 **11.9M+ 次路徑解析/秒（83.9 ns/op）**。

### 4. 端對端密碼學與格式安全 (End-to-End Cryptographic & Format Security) 🔒
*   **OWASP / RFC 9106 強化 Argon2id KDF**：記憶體困難金鑰衍生（`64 MiB`、`t=3`、`p=4`），搭配隨機 Salt 與 **BLAKE2b-512 網域隔離子金鑰階層**（`payload_key`、`filename_siv_key`、`filename_cipher_key`、`ipc_auth_key`），於 Drop 時以 `zeroize` 自動清零。
*   **Context-Bound XChaCha20-Poly1305 AEAD**：透過 Poly1305 AAD 將密文綁定至 `inode_id`（Seekable 分塊另綁定 `chunk_idx` 與 `flags`），防止跨檔置換與分塊重排；大檔與分塊於 AArch64 自動啟用 NEON、x86_64 自動啟用 AVX2 SIMD，短檔名（`<= 192 B`）則走與 RFC 8439 完全一致的 1-block scalar fast path，兼顧大檔吞吐與目錄速度。
*   **192-bit SIV 檔名加密**：確定性 `XChaCha20-Poly1305` 檔名加密（`_e_...`），採完整 24-byte 合成 Nonce 並將 `parent_inode` 納入 Poly1305 AAD 驗證；原始映像檔掃描完全不外洩任何檔名或內容。
*   **縱深防禦磁碟格式強化**：
    *   加密映像檔一律拒絕未加密 Inode 與明文檔名注入，並拒絕以密碼開啟未加密／遭降級的映像檔（`NotEncrypted`）。
    *   `SuperBlock::validate` 強制檢查版本白名單與加密旗標一致性；驗證資料區區塊指標邊界、新配置區塊預先歸零、新建 `.img` 採擁有者專用 `0600` 權限（`create_encrypted` 加 `O_NOFOLLOW`）、CLI 遞迴 `ls -r`/`rm -r` 具循環與深度防護，以及可失敗上限配置（`MAX_READ_DATA_BYTES`、`MAX_DECOMPRESSED_STREAM_BYTES`）防禦偽造超大 `inode.size` OOM 與 Zstd 解壓炸彈。
    *   強化 IPC 傳輸：Challenge-Response 驗證（密碼不經網路傳輸）、`0700`/`0600` socket 與 rendezvous 檔、TCP 預設僅監聽 loopback（`--allow-remote-ipsc`）與有界框架反序列化。

### 5. 科學數值前處理濾鏡與壓縮 (Numerical Data Filters & Compression) ⚡
*   **前處理濾鏡**：一階差分 Delta（含 NEON/SSE2 SIMD）、Byte Shuffle（AoS $\rightarrow$ SoA）、BitShuffle（8×8 位元矩陣轉置）與 TruncPrecision，可將結構化數值資料壓縮比由 1.95x 提升至 **390x（空間節省 99.7%）**；另支援可選 `blosc2` feature。
*   **智慧濾鏡推薦器**：計算候選管線的資訊熵與壓縮比，自動推薦最佳參數。

### 6. 透明多行程與網路協同 IPC (Transparent Multi-Process & Network IPC) 🔄
*   **動態主從架構 (Master-Proxy)**：首個行程取得 OS 級排他鎖（`fcntl`）成為 **Master**；後續行程透過本地 Unix Domain Socket (UDS) 或 Network TCP（`--network`）以連線池透明轉發請求。

### 7. 可插拔異步 I/O 引擎 (Pluggable Async I/O Engine) ⚡
*   可透過 API 或 `OIFS_IO_BACKEND` 動態切換：
    *   `IoBackend::Mmap`：共享記憶體零拷貝與連續 Extent 合併。
    *   `IoBackend::Pread`：合併後連續 Extent 位置讀取。
    *   `IoBackend::IoUring`：Linux 異步提交佇列（可選 `io-uring` feature）。

### 8. 數學形式化驗證與工具鏈 (Mathematical Verification & Tooling) 🛡️
*   **92 項 Kani 數學形式化證明**：使用 AWS **Kani Rust Verifier (CBMC)** 驗證區塊指標解析、SuperBlock 幾何不變量、Bitmap 掃描、濾鏡雙射可逆性、CRC32C 撕裂偵測、WAL 環狀緩衝與冪等性、Seekable 分塊位元隔離與漸進截斷、AAD 網域隔離與 IPC 框架邊界。
*   **Bolero 模糊測試與 Shuttle 並行驗證**：結合 Bolero 屬性／模糊測試、Shuttle 執行緒排程窮舉驗證與 68 組整合測試套件。
*   **Model Context Protocol (MCP) 伺服器 🤖**：內建 `oifs_mcp`（`--features mcp`），讓 AI 代理人透過 JSON-RPC 工具直接操作 OIFS。
*   **穩定 C/C++ ABI (FFI) 🔌**：提供動態函式庫（`liboifs.so` / `liboifs.dylib`）與三態版本握手（`oifs_check_version`、`oifs_loaded_path`）。

---

## 效能與並行實測基準 (Performance & Concurrency Benchmarks)

於 Release 建置（Apple Silicon `aarch64`、`--release`）實測取得：

| 評測場景 | 負載與條件 | 實測結果 / 輸送量 | 亮點 / 對比 |
| :--- | :--- | :--- | :--- |
| **密集雙重間接 `read_at`** | 1,000 × 1 MB 雙重間接區塊讀取 | **58.2 GB/s** (`16.8 ms`) | 批次葉節點解析 + 連續區段合併，**提速 2.63 倍** |
| **稀疏子樹跳洞讀取** | 200 × 4 MB 未配置間接子樹 | **232.5 GB/s** (`3.36 ms`) | 較逐 4 KiB 步進**提速 4.55 倍** |
| **Seekable 64K `read_at`** | 16 MB 分塊檔（`1 MiB` / `4 KiB` 順序讀） | **12.6 GB/s** (`1.27 ms`) / **3.32 GB/s** (`4.82 ms`) | 常駐 Worker Pool + 4-chunk 預取，**提速 2.70x / 1.47x** |
| **大檔 Journaled 寫入** | 32 MB 首次寫入 / 覆寫（開啟 WAL） | **9.78 GB/s** / **46.1 GB/s** | 1 MB–32 MB 線性擴展 |
| **加密 64K Append** | 128 × 64 KiB 加密附加寫入（8 MB） | **701 MB/s** (`11.41 ms`) | AArch64 NEON + 短檔名 scalar fast path，**提速 +21.7%** 且加密目錄零退化 |
| **32 分片目錄快取** | 16 執行緒 × 10k 次操作（97% 查詢 + 3% 建索引） | **8.60M lookups/sec** (`3.24 ms`) | 較單一 `RwLock` **提速 9.97 倍** |
| **路徑解析與目錄列舉** | 20 萬次三層 `resolve_path` / 5,000 檔 `list_dir` | **11.9M lookups/sec** (`83.9 ns`) / **9,523 listings/sec** (`105 µs`) | 零 Heap 配置路徑解析 |
| **Journal 持久化模式** | 500 × 512 B `create` + `write`（開啟 WAL） | Lazy **626k/s** \| RangeAsync **234k/s** \| Strict **10.9k/s** | JBD2 式延遲 Metadata Checkpoint 使 `Strict` **提速 2.31 倍** |
| **原地 COW 磁碟重組** | 500 個碎片化 16 KB 檔案（開啟 WAL） | **3.28–3.65 ms**（免重組：`18.9 µs`） | 字組級 Bitmap + WAL 操作合併，**提速 4.4 倍** |
| **數值資料壓縮效益** | 4-byte 結構化遙測資料（`Delta` + Zstd） | **390.1 倍壓縮比**（省 99.7%） | 較原生 Zstd（`1.95x`）**優化 200 倍** |

---

## 建置與測試 (Build & Test)

```bash
# Release 建置（精簡預設依賴；可選啟用 --features blosc2,mcp,io-uring）
cargo build --release

# 執行全部 68 組單元與整合測試套件
cargo test --release

# 平行執行全部 92 項 Kani 數學形式化證明
cargo kani -j 8 --output-format=terse

# 一鍵執行發布前檢查腳本（fmt、clippy、build、test、shuttle、kani、coverage）
./scripts/release_check.sh --quick
```

---

## 命令列工具使用說明 (CLI Usage)

### 1. 建立映像檔
```bash
# 建立 10MB 映像檔（可加 --journal 啟用 Metadata WAL）
cargo run --bin oifs -- -i disk.img create --size 10 --journal

# 建立加密映像檔（自動安全遮罩密碼輸入）
cargo run --bin oifs -- -i encrypted.img create --size 10 --encrypt
```

### 2. 檔案匯入、匯出與目錄操作
```bash
# 匯入與匯出檔案
cargo run --bin oifs -- -i disk.img put dataset.bin
cargo run --bin oifs -- -i disk.img get dataset.bin extracted.bin

# 建立目錄、遞迴列表、遞迴刪除
cargo run --bin oifs -- -i disk.img mkdir logs
cargo run --bin oifs -- -i disk.img ls -r
cargo run --bin oifs -- -i disk.img rm -r logs
```

### 3. 數值壓縮與智慧濾鏡推薦 ⚡
```bash
# 分析資料特徵並取得最佳濾鏡推薦報告
cargo run --bin oifs -- filter-analyze dataset.bin

# 自動推薦或手動指定濾鏡（delta、shuffle、bitshuffle、both）
cargo run --bin oifs -- -i disk.img put dataset.bin --filter auto
cargo run --bin oifs -- -i disk.img put dataset.bin --filter delta --typesize 4
```

### 4. 結構檢查 (FSCK)、線上重組與格式遷移 🛠️
```bash
# 結構一致性檢查
cargo run --bin oifs -- -i disk.img fsck
cargo run --bin oifs -- -i disk.img fsck --json

# 線上磁碟重組（或使用 --auto-defrag=0.3 啟用背景自動重組）
cargo run --bin oifs -- -i disk.img defrag

# 將舊版 v1 映像檔原地升級為 v2 格式
cargo run --bin oifs -- -i legacy.img migrate
```

### 5. 多行程、網路 IPC 與 MCP 伺服器 🌐🤖
```bash
# 節點 1 以 Master 啟動並監聽 loopback TCP 8989 埠
cargo run --bin oifs -- -i disk.img --network 127.0.0.1:8989 ls

# 節點 2 以 Proxy 連線並透明轉發操作
cargo run --bin oifs -- -i disk.img --network 127.0.0.1:8989 put data.bin

# 啟動原生 MCP 伺服器供 AI 代理人使用
cargo run --bin oifs_mcp --features mcp
```

---

## Rust API 程式碼範例

```rust
use oifs::disk::{CompressionMode, DiskManager};

// 開啟現有映像檔（size 傳入 0 表示不截斷既有檔案）
let dm = DiskManager::open("disk.img", 0).unwrap();

// 解析根目錄並建立檔案
let root_id = dm.resolve_path(".").unwrap();
let file_id = dm.create_file(root_id, "telemetry.bin").unwrap();

// 寫入資料（支援 Never、Auto、Always、Stream { level }、Seekable { chunk_size, level }）
let data = b"High-throughput concurrent payload";
dm.write_data(file_id, 0, data, CompressionMode::Auto).unwrap();

// 讀取完整檔案或指定區段
let content = dm.read_data(file_id).unwrap();
assert_eq!(content, data);
```

---

## 官方文件與互動式視覺化導覽 🌐

探索完整的系統架構規範、模組依賴關係圖與經過形式化驗證的設計宣告：  
👉 **[https://ych.github.io/oifs/](https://ych.github.io/oifs/)**

---

## 授權條款 (License)

Copyright (c) 2026 Yu-Chun Huang <ych@ychuang.org>

本專案採用 Apache License 2.0 條款授權。詳細內容請參閱 [LICENSE](http://www.apache.org/licenses/LICENSE-2.0)。
