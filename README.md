# OIFS (O's Inode File System)

OIFS 是一個使用 Rust 編寫的簡單 Inode 檔案系統實作。它支援基本的檔案操作、目錄管理、並發訪問保護 (Thread-safe)，以及透過 FFI 供 C/C++ 呼叫。

## 功能特色 (Features)

*   **Inode-based Architecture**: 採用標準的 Inode 設計管理檔案與目錄。
*   **Large File Support**: 支援單級間接 (Single Indirect) 與雙級間接 (Double Indirect) 區塊，單一檔案大小上限提升至 **1GB** (取消原本 48KB 限制)。
*   **Encryption Support** 🔒:
    *   XChaCha20-Poly1305 AEAD 加密演算法
    *   Argon2id 密碼金鑰衍生
    *   支援加密與壓縮同時使用
    *   每個檔案使用唯一的 Nonce
    *   **CLI 密碼輸入遮罩**：在終端機輸入密碼時自動隱藏 (Suppress Echo)，防範旁窺安全。
*   **Integrity & Diagnostics (fsck)** 🛠️:
    *   支援完整性掃描，檢查 Orphan Inodes, Leaked Blocks, Missing Blocks 以及 Cross-Linked Blocks，並支援 JSON 與文字格式輸出。
*   **Crash Safety**:
    *   Metadata 操作 (如 `create`, `mkdir`, `delete`) 支援同步寫回 (Sync-on-write)。
    *   使用 `mmap` 的 flush 機制確保資料在崩潰時不遺失。
*   **Concurrency & Multi-Process Session (Master-Proxy IPC)** 🔄:
    *   **動態主從架構 (Dynamic Master-Proxy)**：第一個開啟映像檔的行程成為 Master，持有 OS 層級獨占排他鎖 (`flock`)；後續行程自動轉為 Proxy，所有操作透過 IPC 請求透明轉發至 Master 執行，保證多行程並行安全。
    *   **雙模 IPC 協同**：支援本地高效 Unix Domain Socket (UDS) 與跨主機 Network TCP 模式 (`--network`)。
    *   **行程級 Session 註冊快取**：提供 `OifsSession::get_or_open` 進行安全工作階段重複利用，支援遞迴多跳符號連結（Symlink）路徑正規化。
    *   **區塊級並行合併 (Merge Policy)**：同區塊非重疊區間自動原地合併 (In-place Merge)，重疊區間依序遵循原子性 Last-Writer-Wins (POSIX pwrite 語意)。
*   **Online Safe Defragmentation (磁碟線上重組)** 🧹:
    *   **碎片化程度分析**：提供 `analyze_fragmentation` 精準量測不連續區塊與零散空洞。
    *   **安全 3 步驟原子置換**：採三段式置換並建立 `.old` 備份檔，確保重組過程遭遇斷電或崩潰能安全恢復。
    *   **完整保留 Metadata**：重新分配連續區塊的同時，100% 保留目錄結構、Blosc2 濾鏡參數、Zstd 壓縮與加密資訊。
*   **Model Context Protocol (MCP) Server 支援** 🤖:
    *   內建 `oifs_mcp` 二進制檔，無縫對接 Claude Desktop, Cursor, Antigravity 等 AI 工具，支援透過標準 MCP JSON-RPC 協議操作 OIFS 映像檔。
*   **Extreme Zero-Allocation Optimization (極致效能重構)** 🚀:
    *   **Bitmap 區塊分配掃描**：改採 64-bit 字組搭配硬體指令 (`tzcnt`)，區塊搜尋提速 **11x ~ 13.4x**。
    *   **零記憶體配置目錄查找**：以切片直接原地比對標頭與檔名，目錄搜尋提速 **12x ~ 26.8x**，Heap 分配次數降為 0。
    *   **步進跳躍插入點掃描**：新增檔案與目錄時以 `18 + len` 步進切片跳躍，提速 **32.5x**。
    *   **讀寫路徑零拷貝**：引入 `Cow<'a, [u8]>`，常規讀寫完全免除記憶體複製與分配，提速 **10x ~ 3,162x**。
*   **嚴格的並行驗證與壓力測試 (Concurrency Verification)** 🧪:
    *   整合 **Shuttle** 隨機執行緒排程探索測試，窮舉並發 race condition 與死鎖。
    *   整合 **ThreadSanitizer (TSan)** 進行極限多行程高頻壓力測試。
*   **CLI Tool**: 提供完整的命令列工具進行映像檔操作。
*   **Blosc2 Pre-compression Data Filters & Extreme Compression** ⚡:
    *   **為何引入 Blosc2**：傳統通用壓縮算法 (如 Zstandard, LZ4) 基於字節滑動窗口 (LZ77)，對文字重複串效果佳，但對連續數值流 (Float32/64, Int32/64)、時間序列、結構體陣列 (AoS) 壓縮比極低。引入 Blosc2 前處理濾鏡能在壓縮前重組位元組或計算相鄰增量，大幅削減資訊熵 (Shannon Entropy)，使連續整數數列壓縮比從 1.95x 飆升至 **390x** (空間節省率達 **99.7%**)。
    *   **支援濾鏡**：First-order Delta (一階差分)、Byte Shuffle (位元組轉置)、BitShuffle (位元級轉置)、TruncPrecision (浮點數精度截斷)。
    *   **複合濾鏡管線 (Composite Pipeline)**：支援任意順序的多重濾鏡堆疊串聯。
    *   **智慧推薦工具 (Filter Recommendation Tool)**：自動量測資料資訊熵並平行模擬評估 14 種濾鏡組合，輸出壓縮效益排行榜與建議參數。
    *   **原生 C-Blosc2 整合**：支援直接呼叫原生 `blosc2` C 函式庫 Chunk 編碼解碼器。
*   **Formal Verification Guarantee (形式化數學驗證)** 🛡️:
    *   使用 AWS **Kani Rust Verifier (CBMC/CaDiCaL)** 建立 **25 項數學證明**，覆蓋濾鏡雙射可逆性、二補數環繞溢位安全、Superblock 邊界與區塊配置無碰撞。
*   **C API (FFI)** 🔌: 提供極為完整的 C 語言介面庫 (`liboifs.so`)，支援加密開啟、檔案讀寫、目錄建立、`oifs_get_or_open` 工作階段快取以及詳細錯誤診斷輸出。

## 建置 (Build)

```bash
# 建置 Rust 專案
cargo build --release

# 執行測試
cargo test
```

## 使用說明 (CLI Usage)

您可以使用編譯出的 `oifs` 執行檔來管理檔案系統映像檔 (Image)。

### 1. 建立映像檔 (Create Image)
建立一個 10MB 的檔案系統映像檔：
```bash
cargo run --bin oifs -- -i disk.img create --size 10
```

#### 建立加密映像檔 (Create Encrypted Image) 🔒
建立一個加密的檔案系統（會提示輸入密碼）：
```bash
cargo run --bin oifs -- -i encrypted.img create --size 10 --encrypt
```

使用 `--password` 參數直接指定密碼（不建議用於生產環境）：
```bash
cargo run --bin oifs -- -i encrypted.img --password mypassword create --size 10 --encrypt
```

### 2. 匯入檔案 (Import File)
將本機檔案 `hello.txt` 匯入到映像檔中：
```bash
touch hello.txt && echo "Hello World" > hello.txt
cargo run --bin oifs -- -i disk.img put hello.txt
```

**加密檔案系統會自動偵測並提示輸入密碼**：
```bash
cargo run --bin oifs -- -i encrypted.img put hello.txt
# 🔒 Encrypted filesystem detected. Enter password: 
```

### 3. 建立目錄 (Make Directory)
在映像檔中建立一個新目錄：
```bash
cargo run --bin oifs -- -i disk.img mkdir documents
```

### 4. 列出檔案 (List Files)
列出根目錄下的檔案與資料夾 (支援遞迴 `-r`)：
```bash
cargo run --bin oifs -- -i disk.img ls -r
```

### 5. 匯出檔案 (Export File)
從映像檔中讀取檔案並存回本機：
```bash
cargo run --bin oifs -- -i disk.img get hello.txt downloaded.txt
```

### 6. 一致性檢查 (Filesystem Consistency Check - FSCK) 🛠️
掃描並確認映像檔結構完整性，偵測是否有孤立 Inode、洩漏區塊或多重引用：
```bash
cargo run --bin oifs -- -i disk.img fsck
```

支援以 JSON 格式輸出：
```bash
cargo run --bin oifs -- -i disk.img fsck --json
```

### 7. Blosc2 濾鏡智慧推薦與數值壓縮 (Filter Recommendation & Put) ⚡

#### 📊 獨立分析檔案並取得最佳濾鏡推薦：
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
Recommendation Rationale: Data displays strong linear/temporal correlation; first-order delta collapses dynamic range, shrinking entropy.
Recommended Blosc2 Filter(s): ["blosc2::Filter::Delta"]
Command to import with recommended filter:
  oifs -i disk.img put "dataset.bin" --filter delta --typesize 4
```

#### 🚀 自動依據分析結果套用最佳濾鏡匯入：
```bash
cargo run --bin oifs -- -i disk.img put dataset.bin --filter auto
```

#### 🛠️ 手動指定特定濾鏡與 Element Typesize (1, 2, 4, 8 bytes)：
```bash
# 一階差分 (Delta)
cargo run --bin oifs -- -i disk.img put dataset.bin --filter delta --typesize 4

# 位元組轉置 (Byte Shuffle)
cargo run --bin oifs -- -i disk.img put dataset.bin --filter shuffle --typesize 4

# 位元級轉置 (BitShuffle)
cargo run --bin oifs -- -i disk.img put dataset.bin --filter bitshuffle --typesize 4

# 複合濾鏡 (Delta + ByteShuffle)
cargo run --bin oifs -- -i disk.img put dataset.bin --filter both --typesize 4
```

### 8. 磁碟線上重組 (Online Defragmentation) 🧹
當檔案系統頻繁建立與刪除檔案後，可執行線上重組以消除碎片並將零散區塊整理為連續區塊：
```bash
cargo run --bin oifs -- -i disk.img defrag
```
系統會自動輸出重組前後的碎片率對比（例如 `Fragmentation: 45.2% -> 0.0%`），並在安全 3 步驟機制下完成原子切換。

### 9. 跨行程與網路協同模式 (Network Mode IPC) 🌐
OIFS 支援多行程與跨主機協同。可透過 `--network` 參數指定監聽/連線的 TCP 位址：
```bash
# 第一個行程作為 Master 啟動並監聽 TCP 8989 埠
cargo run --bin oifs -- -i disk.img --network 127.0.0.1:8989 ls

# 其他行程或節點自動作為 Proxy 透明轉發請求
cargo run --bin oifs -- -i disk.img --network 127.0.0.1:8989 put data.bin
```

### 10. Model Context Protocol (MCP) 伺服器 🤖
OIFS 提供專用的 MCP 伺服器二進位檔 `oifs_mcp`，讓 AI Agent 與 IDE 能直接以標準協議操作 OIFS：
```bash
cargo run --bin oifs_mcp --features mcp
```
支援的 Tool 呼叫包含 `create_image`, `list_files`, `read_file`, `write_file`, `delete_file`, `fsck`, `defrag`。

### 11. 效能對比基準測試 (Performance Proof Benchmark) 📊
您可以隨時執行專案內建的 Release 模式效能對比測試，親自檢驗各項熱點路徑在重構後的實測提速倍率：
```bash
cargo test --test perf_comparison --release -- --nocapture
```


## Blosc2 與前處理濾鏡技術說明 (Why Blosc2?)

### 1. 為何要引入 Blosc2？
在科學計算、HPC 與機器學習環境中，我們處理的資料大多不是 ASCII 文字，而是二進位數值（如 32-bit/64-bit 浮點數、時間序列整數、感測器讀數、地理座標等）。

傳統壓縮演算法（如 Zstandard、LZ4）主要基於字典匹配（LZ77）與熵編碼（Huffman/FSE）：
* 當處理純文字時，重複出現的單字或標籤能輕易被壓縮。
* 但數值資料在記憶體中是以連續二進位表示（如 IEEE 754 浮點數），其指數位元與小數位元交錯，即使數值非常接近，位元組層級也難以找到重複的子字串。這導致未經處理的數值資料送入 Zstd 時，壓縮比往往只有 1.2x ~ 2.0x。

**Blosc2（以及其前處理濾鏡架構）的核心使命**：
> 在壓縮前先透過「可逆轉換」重整資料排布，將高資訊熵的二進位資料轉化為大量重複連續零或低動態範圍差分，從根本上**瓦解資訊熵**，讓後續的壓縮演算法發揮數十倍甚至數百倍的壓縮效益。

### 2. 核心濾鏡原理
* **Delta (一階差分)**：
  計算相鄰元素間的差值：$\Delta[0] = x[0], \Delta[i] = x[i] \mathbin{\text{wrapping\_sub}} x[i-1]$。
  在連續變化或趨勢數列中，原本跨越很大動態範圍的數值（如 1000000, 1000001, 1000002）會被全部轉換為 `1`，釋放極高壓縮比。
* **Byte Shuffle (位元組轉置)**：
  將結構體陣列（Array of Structures, AoS）重排為結構陣列（Structure of Arrays, SoA）。
  將所有元素的第 0 個 Byte 集中、第 1 個 Byte 集中...使高有效位的連續零群聚成超長連續字節串。
* **BitShuffle (位元級轉置)**：
  進行 $8 \times 8$ bit 矩陣轉置。對稀疏矩陣（Sparse Matrix）與二元布林遮罩（Boolean Array）具備比 Byte Shuffle 更強大的點陣聚集能力。
* **TruncPrecision (浮點數精度截斷)**：
  將 Float32/Float64 尾數（Mantissa）低有效位清零，抹除不具物理意義的噪聲位元，大幅提升浮點數壓縮比。

### 3. 複合濾鏡管線 (Composite Filter Pipeline)
支援使用者自選並自由堆疊任意順序的濾鏡：
```rust
use oifs::filters::{FilterPipeline, FilterType};

let pipeline = FilterPipeline::new(4)
    .then(FilterType::TruncPrecision { prec_bits: 14 })
    .then(FilterType::Delta)
    .then(FilterType::ByteShuffle)
    .then(FilterType::BitShuffle);

let filtered = pipeline.apply(&data);
let restored = pipeline.unapply(&filtered);
```

### 4. 原生 C-Blosc2 整合呼叫
若欲直接調用底層已編譯的 C-Blosc2 原生庫：
```rust
use oifs::filters::{blosc2_compress, blosc2_decompress};
use blosc2::{Filter, CompressAlgo};

let compressed = blosc2_compress(&data, 4, &[Filter::BitShuffle], CompressAlgo::Lz4, 5)?;
let decompressed = blosc2_decompress(&compressed)?;
```

### 5. 形式化驗證保證 (Formal Verification with Kani)
所有純 Rust 濾鏡實作皆透過 AWS **Kani Rust Verifier (CBMC/CaDiCaL)** 完成形式化數學證明（共 25 個 Proof Harness 全部通過）：
* 證明二補數溢位環繞下 Delta 嚴格可逆且不 panic。
* 證明任意符號化位元組序列經 Shuffle / BitShuffle 運算皆完全雙射還原。
* 證明任意非對齊尾部位元組（Tail Bytes）不被吞噬或錯位。

---

## Rust API 範例

若要在其他 Rust 專案中使用 OIFS：

```rust
use oifs::disk::DiskManager;
use std::path::Path;

// 開啟映像檔 (size 設為 0 表示開啟現有檔案)
let dm = DiskManager::open("disk.img", 0).unwrap();

// 解析根目錄
let root_id = dm.resolve_path(".").unwrap();

// 建立檔案 (返回 Inode ID)
let file_id = dm.create_file(root_id, "test.txt").unwrap();

// 寫入資料 (支援 Offset)
let data = b"Hello OIFS";
dm.write_data(file_id, 0, data).unwrap();

// 讀取資料
let content = dm.read_data(file_id).unwrap();
assert_eq!(content, data);
```

## 系統架構

*   **SuperBlock**: 儲存檔案系統 Metadata (Magic Code, Size, Bitmaps locations)。
*   **Inode Bitmap & Data Bitmap**: 管理 Inode 與 Data Block 的分配狀態。
*   **Inode Table**: 儲存所有 Inode 結構 (Mode, Size, Block pointers)。
*   **Data Blocks**: 實際儲存檔案內容或目錄項目 (Directory Entries)。
*   **Directory Entry**: 包含 `inode_id`, `name`, `hash`。
*   **Dynamic Master-Proxy IPC**: 協調跨行程、跨節點對單一映像檔的並發訪問。

## 並行模型與區塊合併策略 (Concurrency Model & Merge Policy) ⚖️

當多個行程（Processes）或多執行緒同時存取同一個檔案系統映像檔時，OIFS 透過兩層架構保證資料一致性與 POSIX 語意相容：

### 1. Master-Proxy 協同架構
* **獨占協調**：首個開啟映像檔的行程成為 Master，持有 OS 層級排他鎖（`flock`）與 `mmap` 的唯一修改權。
* **透明代理**：後續行程自動成為 Proxy，透過 IPC Socket（Unix Domain Socket 或 TCP）將操作發送至 Master。
* **嚴格循序化**：Master 內部透過 `Mutex<DiskManagerInner>` 將並行寫入操作循序化處理，徹底杜絕底層磁碟區塊的記憶體競爭與損壞。

### 2. 同檔案同區塊的合併策略 (Block Merge Policy)
當多個行程寫入**同一檔案的同一個 4KB 區塊**時，OIFS 的合併原則如下：

| 衝突場景 | 合併策略 (Merge Policy) | 行為說明與最終狀態 |
| :--- | :--- | :--- |
| **同 Block、不重疊位移 (Disjoint Slices)** | **In-place Byte Merging (原地合併)** | 例如 Process A 寫 `0..100`，Process B 寫 `200..300`。兩者在 4KB 切片中各自寫入自己的 offset，未觸及的 byte 原樣保留，**雙方資料完美合併共存**。 |
| **同 Block、重疊位移 (Overlapping Slices)** | **Last-Writer-Wins (原子性後寫者勝)** | 重疊部分依 Master Mutex 獲取順序，由後寫入者原子性覆蓋；單次寫入受 Mutex 保護，**絕不產生位元撕裂 (No Torn Writes)**。符合標準 POSIX `pwrite()` 語意。 |
| **同 Block、已壓縮檔案 (Compressed File)** | **Whole-File Replace Only (拒絕部分追加)** | 壓縮檔案具全局上下文相依性，系統自動防禦拒絕非 0 offset 的部分寫入；若從 offset 0 寫入則原子性覆寫整個壓縮流。 |

## 測試 (Testing)

專案包含超過 50 項完整的測試套件與形式化數學證明：
*   **Unit Tests**: 基本模組功能測試（Superblock、Inode、Directory、Allocator 等）。
*   **Integration Tests**: 基礎與擴充整合測試。
*   **Large File Test**: 驗證大檔案極限（1MB 以上）的單/雙級間接區塊讀寫。
*   **FSCK Extended Test**: 驗證孤立 Inode、洩漏區塊、遺失區塊與交叉引用之診斷偵測。
*   **Online Defrag Test**: 驗證碎片分析、3 步驟安全原子重組與 Metadata 完整保留。
*   **Shuttle Concurrency Tests**: 使用 **Shuttle** 隨機交錯排程探索，驗證多執行緒競態與死鎖安全。
*   **ThreadSanitizer (TSan) Stress Tests**: 在高頻高壓下驗證跨執行緒讀寫與區塊合併完整性。
*   **Session IPC & Edge Cases Tests**: 驗證 Master-Proxy 行程切換、Socket 自癒、跨行程檔案生命週期與突發並發。
*   **Network Multi-Node Sync Tests**: 驗證多節點跨 TCP 連線在單一檔案上的並發切片寫入與資料同步。
*   **MCP Server Tests**: 驗證符合 Model Context Protocol 規範之 JSON-RPC 工具呼叫。
*   **Performance Comparison Microbenchmark**: 實測驗證重構後之 Zero-Allocation 與演算法加速倍率。
*   **Kani Formal Proofs**: 25 項數學證明，保證濾鏡雙射可逆性與區塊配置無溢位。

執行所有標準測試：
```bash
cargo test
```

執行 Release 模式效能對比驗證：
```bash
cargo test --test perf_comparison --release -- --nocapture
```


## 加密安全性說明 (Encryption Security)

### 加密演算法
*   **AEAD Cipher**: XChaCha20-Poly1305
    *   提供機密性（Confidentiality）和完整性（Integrity）保護
    *   192-bit Nonce，每個檔案使用唯一的隨機 Nonce
    *   256-bit 金鑰
*   **金鑰衍生**: Argon2id
    *   記憶體困難（Memory-hard）演算法，抵抗暴力破解
    *   每個檔案系統使用唯一的 128-bit 隨機 Salt
    *   Salt 儲存在 SuperBlock 中

### 安全性考量
1. **密碼強度**: 建議使用至少 12 個字元的強密碼，包含大小寫字母、數字和符號
2. **密碼遺失**: 密碼不會儲存在磁碟上，**遺失密碼將無法恢復資料**
3. **⚠️ 已知限制 — 目錄 metadata 未加密**: 目前 `--encrypt` 只保護**檔案內容**。
   目錄項目（檔名、inode id、size、modified time）仍以明文寫入 data block，
   因此持有 image 的人即使沒有密碼，也能 `ls` 列出檔名與大小。如果你的
   威脅模型在意檔名洩漏，請額外避免具識別性的命名（例如改用 hash 為名）。
   完整 directory-block encryption 是 on-disk format 變更，將在後續版本處理。
4. **記憶體安全**:
   *   加密金鑰使用 `zeroize` crate 在釋放時自動清零
   *   但 Rust 無法保證記憶體不會被交換到磁碟（swap）
   *   建議使用加密的 swap 或停用 swap 以獲得最大安全性
5. **Nonce 唯一性**: 每個檔案使用密碼學安全隨機數產生器（CSPRNG）產生唯一 Nonce
6. **加密與壓縮**: 資料先壓縮後加密，確保壓縮效率不受影響

### 效能影響
*   加密/解密操作會增加約 5-15% 的讀寫延遲（取決於檔案大小）
*   Argon2 金鑰衍生在開啟檔案系統時執行一次（約 100-500ms）
*   加密不會影響壓縮率

### 驗證加密
您可以使用 `hexdump` 驗證資料確實被加密：
```bash
# 建立加密檔案系統並寫入資料
cargo run --bin oifs -- -i encrypted.img create --size 10 --encrypt
echo "SECRET_DATA" > test.txt
cargo run --bin oifs -- -i encrypted.img put test.txt

# 檢查原始磁碟映像（應該找不到明文）
hexdump -C encrypted.img | grep "SECRET_DATA"  # 應該沒有結果
```
