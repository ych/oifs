# OIFS (O's Inode File System)

[English](README.md) | [繁體中文](README_zh.md) | [📖 線上官方文件](https://ych.github.io/oifs/)

> 🌐 **線上官方文件與架構視覺化網站**: [https://ych.github.io/oifs/](https://ych.github.io/oifs/)  
> 探索完整的互動式系統架構圖形導覽、模組呼叫關係、形式化數學證明與技術規格文件。

OIFS 是一個以 Rust 打造的高效能、嵌入式、多行程 Inode 檔案系統引擎。具備斷電崩潰自癒（Crash Resilience）、細粒度並行存取保護（Fine-Grained Concurrency）、軍規級 AEAD 加密、科學數值前處理壓縮濾鏡（Blosc2）、可插拔異步 I/O 引擎、專為 AI 智慧代理人設計的原生 Model Context Protocol (MCP) 伺服器，以及穩定的 C/C++ FFI 介面。

---

## 核心功能與系統架構 (Capabilities & Architecture)

### 1. 儲存與大容量擴展 (Storage & Scaling)
*   **類 Unix Inode 架構**：標準 Inode 階層架構，管理常規檔案、目錄樹、存取權限與奈秒級時間戳記。
*   **超大檔案支援（高達 513 GB）**：具備單級、雙級與三級間接區塊索引（134,480,394 個 4KB 區塊），且 100% 向後相容舊版磁碟格式。
*   **動態多區塊目錄 (Multi-Block Directory)**：目錄容量隨項目自動跨動態分配的 4KB Extent 區塊擴展，支援單一目錄容納數萬筆項目，並搭配記憶體加速快取索引。
*   **線上零停機磁碟重組 (Online Defragmentation) 🧹**：`analyze_fragmentation` 精準計算碎片率；線上重組採用安全的三步驟原子置換機制與 `.old` 備份防護，在連續區塊重配的過程中 100% 完整保留 Metadata、濾鏡參數與加密資訊。
*   **FSCK 結構完整性診斷 🛠️**：全域結構一致性掃描器，能精準偵測孤立 Inode、洩漏區塊、遺失區塊與交叉參照，並支援人類可讀文字與結構化 JSON 輸出。

### 2. 事務性斷電崩潰自癒與持久化 (Transactional Crash Resilience & Durability)
*   **事務性 Metadata WAL (預寫日誌)**：針對目錄變更與檔案區塊配置採用環狀緩衝區（Circular Ring Buffer）預寫日誌。所有結構性變更在實體區塊寫入前皆先完成事務日誌持久化。
*   **即時崩潰復原 (Instant Crash Recovery)**：掛載時自動重放（Replay）未檢查點的事務，並安全捨棄撕裂或不完整的事務，絕不洩漏區塊或殘留孤立 Inode。
*   **可設定持久化策略 (Configurable Durability Policies)**：
    *   `Strict`：每筆事務提交時同步執行 `msync(MS_SYNC)`，提供最高的抗斷電安全保障。
    *   `RangeAsync`：異步分頁寫回並自動進行 4KB 分頁區間合併（Range-Coalescing），減少高達 99.6% 的系統呼叫開銷。
    *   `Lazy`：記憶體緩衝寫回，提供極限記憶體級輸送量。
*   **非阻塞 Flush 並行保護 (Non-Blocking Flush Concurrency)**：`flush()` 與 `flush_async()` 在持有共享讀鎖（Shared Read-Lock）下，透過專屬同步互斥鎖序列化實體 `msync` 呼叫，保證背景寫回磁碟絕不阻礙並行的讀取執行緒。

### 3. 高並行多執行緒引擎 (High-Concurrency Multi-Threaded Engine)
*   **32 分片條帶化讀寫鎖 Inode 快取 (32-Shard Lock-Striped Inode Cache)**：將記憶體中的有限 Inode 快取解耦為 32 個獨立分片，每個分片由各自的 `RwLock` 保護。Inode ID 透過 64 位元費氏雜湊雙射（Fibonacci Hashing Bijection）均勻分佈，徹底消除快取未命中與淘汰時的鎖競爭（**4,000,000+ ops/sec**）。
*   **無鎖處理管線 (Out-of-Lock Processing Pipeline)**：高 CPU 開銷的 Delta/Shuffle 濾鏡運算、Zstandard 壓縮與 XChaCha20 加密全程在檔案系統鎖之外執行。在多 MB 的繁重寫入過程中，讀取執行緒**零飢餓（Zero Starvation）**，讀取延遲維持在 0 µs p50/p99。
*   **解耦目錄變更 (Decoupled Directory Mutations)**：建立檔案與目錄（`create_file`）及刪除（`delete_file`）時，路徑解析、碰撞檢查與檔名加密均在共享讀鎖或無鎖環境下進行，僅在最後的極短區塊提交階段獲取排他寫鎖。
*   **零配置路徑解析 (Zero-Allocation Path Splitting)**：零 Heap 配置的路徑分量迭代器（`resolve_path_iter`）與 $O(1)$ 父目錄切分（`resolve_parent`），提供 **7.7M+ 次路徑解析/秒** 的超高效能。

### 4. 端對端密碼學安全 (End-to-End Cryptographic Security) 🔒
*   **XChaCha20-Poly1305 AEAD**：認證加密與關聯資料保護，提供頂級機密性與防竄改完整性保證。
*   **Argon2id 金鑰衍生**：記憶體困難（Memory-hard）密碼雜湊，搭配儲存於 SuperBlock 的隨機 Salt，能有效抵禦 GPU/ASIC 暴力破解。
*   **合成 IV (SIV) 檔名級別加密**：採用類 fscrypt 的 Synthetic IV 確定性認證加密（ChaCha20-Poly1305 + Blake2b-512 PRF），結合父目錄 Inode 作為 Tweak 與 Base64URL 編碼。原始磁碟掃描（`strings`、`hexdump`）完全無法得知任何真實檔名或目錄結構。
*   **記憶體自動清零 (Zeroization)**：敏感密碼學金鑰在生命週期結束（Drop）時，自動透過 `zeroize` 安全清除記憶體。
*   **CLI 密碼輸入遮罩**：終端機提示輸入密碼時自動隱藏回顯（Suppress Echo），防範旁窺外洩。

### 5. 科學數值前處理濾鏡與 Blosc2 極限壓縮 (Numerical Data Filters & Blosc2 Compression) ⚡
*   **為何需要前處理濾鏡**：傳統通用壓縮算法（Zstandard、LZ4）基於滑動視窗字節匹配（LZ77），對二進位數值陣列（IEEE 754 浮點數、時間序列整數、向量座標）壓縮效果有限。前處理濾鏡在壓縮前重組位元組排布以大幅瓦解夏農資訊熵（Shannon Entropy），使壓縮比從原本的 1.95x 飆升至 **390x（空間節省率達 99.7%）**。
*   **支援濾鏡**：一階差分 Delta（`wrapping_sub`）、位元組轉置 Byte Shuffle（AoS 轉 SoA）、位元級轉置 BitShuffle（$8 \times 8$ 位元矩陣轉置）與浮點精度截斷 TruncPrecision。
*   **複合濾鏡管線 (Composite Pipelines)**：支援任意濾鏡的串聯堆疊。
*   **智慧推薦分析器**：平行計算候選管線的資訊熵與壓縮比，自動推薦最佳濾鏡參數。

### 6. 透明多行程與網路協同 IPC (Transparent Multi-Process & Network IPC) 🔄
*   **動態主從架構 (Dynamic Master-Proxy Coordination)**：首個開啟映像檔的行程獲得 OS 級排他檔案鎖（`flock`）並成為 **Master**；後續行程自動切換為 **Proxy**，透過 IPC 透明轉發所有操作。
*   **雙傳輸後端**：支援極低延遲的本地 Unix Domain Socket (UDS) 與跨主機 Network TCP 模式（`--network`）。
*   **原子性區塊合併策略 (Block-Level Merge Policy)**：同一 4KB 區塊中不重疊的位移區間直接原地合併；重疊區間嚴格遵循原子性後寫者勝（Last-Writer-Wins，相容 POSIX `pwrite` 語意）。

### 7. 可插拔異步 I/O 引擎 (Pluggable Async I/O Engine) ⚡
*   支援透過 API 或環境變數 `OIFS_IO_BACKEND` 於執行階段動態切換底層讀取引擎：
    *   `IoBackend::Mmap`：直接共享記憶體映射零拷貝。
    *   `IoBackend::Pread`：連續 Extent 位置系統呼叫。
    *   `IoBackend::IoUring`：Linux 核心異步提交佇列與核心輪詢。

### 8. 數學形式化驗證與工具鏈 (Mathematical Verification & Tooling) 🛡️
*   **50+ 項 Kani 數學形式化證明**：使用 AWS **Kani Rust Verifier (CBMC/CaDiCaL)** 橫跨 9 大模組完成數學證明，涵蓋整數溢位安全、濾鏡雙射可逆性、環狀緩衝區環繞不變量與檔案覆寫邊界。
*   **Shuttle & TSan 並行驗證**：透過 Shuttle 隨機窮舉執行緒排程交錯測試與 ThreadSanitizer 高頻壓力測試，杜絕資料競爭與死鎖。
*   **Model Context Protocol (MCP) 伺服器 🤖**：內建 `oifs_mcp` 二進位檔，讓 AI 代理人（Claude Desktop、Cursor、Antigravity）透過標準 JSON-RPC 工具呼叫直接檢視與管理 OIFS。
*   **穩定 C/C++ ABI (FFI) 🔌**：提供動態函式庫（`liboifs.so`）與三態版本握手機制，嚴格驗證執行階段相容性。

---

## 效能與並行實測基準 (Performance & Concurrency Benchmarks)

以下效能數據皆於 Release 建置下進行多執行緒極限壓力測試實測取得：

| 評測場景 (Benchmark Scenario) | 負載與測試條件 (Workload / Configuration) | 實測結果 / 輸送量 (Result / Throughput) | 基準對比 (Baseline Comparison) |
| :--- | :--- | :--- | :--- |
| **Inode Cache 輸送量** | 16 個讀取執行緒，跨 3,000 個檔案執行 32,000 次操作（高頻快取未命中與淘汰） | **3,996,081 ops/sec** (總耗時 8.05 ms) | **提升 2.70 倍** (輸送量較全域鎖 1.48M ops/sec 增加 +170%) |
| **繁重寫入下的讀取延遲** | 8 個執行緒讀取 64KB 檔案，同時 2 個寫入執行緒持續壓縮/加密數 MB 檔案 | **p50 = 0 µs, p99 = 0 µs** (累計完成 341,902 次讀取) | **讀取零飢餓 (Zero Starvation)** (原本每次寫入需停頓 30~50 ms) |
| **路徑解析效能** | 10,000 次查詢遍歷多層目錄結構路徑 | **7,729,979 lookups/sec** (單次 129.37 ns) | **完全零 Heap 記憶體配置** |
| **大目錄檢視清單** | 遍歷包含 5,000 個檔案之單一目錄 | **4,712 listings/sec** (單次 212.2 µs) | **提速 2.34 倍** (藉由記憶體索引提升 +134% 輸送量) |
| **多區塊持久化同步** | 1 MB 連續寫入（256 個酬載區塊），啟用 `RangeAsync` 模式 | **1,885 writes/sec** (達 Lazy 極限模式 81% 速度) | **提速 7.03 倍** (輸送量提升 +603%，系統呼叫減少 99.6%) |
| **數值資料壓縮效益** | 4-byte 結構化數值 / 遙測時間序列資料集 | **390.1 倍壓縮比** (空間節省率 99.7%) | 較未濾鏡之原生 Zstd (1.95x) **優化 200 倍** |

---

## 建置與測試 (Build & Test)

```bash
# 建置專案 (Release 模式)
cargo build --release

# 執行所有測試套件
cargo test --all-targets

# 執行高並行實測基準測試
cargo test --test rwlock_concurrency_test -- --nocapture
```

---

## 命令列工具使用說明 (CLI Usage)

編譯出的 `oifs` 執行檔提供完整的 CLI 工具管理檔案系統映像檔。

### 1. 建立映像檔 (Create Image)
建立標準 10MB 映像檔：
```bash
cargo run --bin oifs -- -i disk.img create --size 10
```

建立加密映像檔（終端機會自動安全遮罩密碼輸入）：
```bash
cargo run --bin oifs -- -i encrypted.img create --size 10 --encrypt
```

### 2. 檔案匯入與匯出 (Import & Export)
將本機檔案匯入至映像檔：
```bash
cargo run --bin oifs -- -i disk.img put dataset.bin
```

從映像檔擷取並匯出檔案至本機：
```bash
cargo run --bin oifs -- -i disk.img get dataset.bin extracted.bin
```

### 3. 目錄建立與列表 (Directories & Listings)
```bash
# 建立目錄
cargo run --bin oifs -- -i disk.img mkdir logs

# 遞迴列出所有目錄與檔案
cargo run --bin oifs -- -i disk.img ls -r
```

### 4. Blosc2 數值壓縮與智慧推薦 (Blosc2 Numerical Compression & Recommendation) ⚡
分析資料特徵並取得最佳濾鏡管線推薦報告：
```bash
cargo run --bin oifs -- filter-analyze dataset.bin
```

輸出範例：
```text
=== OIFS Filter Recommendation Report for "dataset.bin" ===
Original Size:        8192 bytes
Baseline Zstd Size:   4199 bytes (Entropy: 4.024 bits/byte)
--------------------------------------------------------------------------------
Filter Pipeline                     Entropy  Zstd Size      Ratio    Savings
--------------------------------------------------------------------------------
None (Raw Zstd)                       4.024       4199      1.95x      48.7%
Delta (typesize=4, u32/f32)           0.811         21    390.10x      99.7% [*RECOMMENDED*]
BitShuffle (typesize=4, u32/f32)      1.122        147     55.73x      98.2%
Shuffle (typesize=4, u32/f32)         4.024        309     26.51x      96.2%
--------------------------------------------------------------------------------
Recommended Blosc2 Filter(s): ["blosc2::Filter::Delta"]
```

使用自動推薦濾鏡匯入檔案：
```bash
cargo run --bin oifs -- -i disk.img put dataset.bin --filter auto
```

或手動指定特定濾鏡（`delta`、`shuffle`、`bitshuffle`、`both`）：
```bash
cargo run --bin oifs -- -i disk.img put dataset.bin --filter delta --typesize 4
```

### 5. 結構完整性檢查 (FSCK) 🛠️
```bash
cargo run --bin oifs -- -i disk.img fsck
cargo run --bin oifs -- -i disk.img fsck --json
```

### 6. 線上零停機磁碟重組 (Online Defrag) 🧹
```bash
cargo run --bin oifs -- -i disk.img defrag
```

### 7. 多行程與網路叢集協同 (Multi-Process & Network Cluster Access) 🌐
```bash
# 節點 1 作為 Master 啟動並監聽 TCP 8989 埠
cargo run --bin oifs -- -i disk.img --network 127.0.0.1:8989 ls

# 節點 2 作為 Proxy 連線並透明轉發寫入操作
cargo run --bin oifs -- -i disk.img --network 127.0.0.1:8989 put data.bin
```

### 8. Model Context Protocol (MCP) 伺服器 🤖
啟動專為 AI 代理人（Claude Desktop、Cursor、Antigravity）設計的原生 MCP 伺服器：
```bash
cargo run --bin oifs_mcp --features mcp
```

---

## Rust API 程式碼範例

```rust
use oifs::disk::{CompressionMode, DiskManager};

// 開啟現有映像檔 (size 傳入 0 表示開啟現有檔案而不截斷)
let dm = DiskManager::open("disk.img", 0).unwrap();

// 解析根目錄
let root_id = dm.resolve_path(".").unwrap();

// 建立檔案 (返回 Inode ID)
let file_id = dm.create_file(root_id, "telemetry.bin").unwrap();

// 寫入資料 (支援 Offset 與壓縮模式)
let data = b"High-throughput concurrent payload";
dm.write_data(file_id, 0, data, CompressionMode::Auto).unwrap();

// 讀取檔案資料
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
