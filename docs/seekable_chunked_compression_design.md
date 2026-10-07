# OIFS Seekable Chunked Compression & Explicit Rewind Policy Design
可隨機回退之分區壓縮與自訂 Zstd 等級架構設計規範

## 1. 核心動機與問題分析 (Motivation & Problem Statement)

在目前 OIFS 的實作中，檔案壓縮採用**全檔連續串流 / 多幀追加模式 (Whole-File / Concatenated Multi-Frame Streaming)**：
* **純追加寫入 (`file_offset == inode.size`)**：
  直接在實體末端追加獨立的 Zstd Frame (`WriteCase::CompressedAppend`)，時間複雜度為 $O(\Delta N)$，吞吐量極高。
* **回退寫入 (`file_offset < inode.size`，歷史資料覆寫 / 隨機寫入)**：
  必須走 `WriteCase::Recompress`（Read-Modify-Recompress），需要：
  1. 解壓全檔到記憶體 ($O(N)$)
  2. 覆寫修改片段
  3. 從 Offset 0 起將整份檔案以 Zstd 重新壓縮 ($O(N)$)
  4. 重新分配區塊並更新 Inode。
  若檔案大小為 100MB，即使僅回退覆寫 16 bytes，也會引發 100MB 的 CPU 壓縮風暴與巨大的寫入放大 (Write Amplification)。
* **隨機片段讀取 (`read_at`, `file_offset < inode.size`)**：
  現行架構下必須將整個壓縮流解壓至記憶體後再進行 slice，帶來不必要的 $O(N)$ CPU 與記憶體開銷。

---

## 2. 核心架構決策：寫檔時主動宣告 (Explicit Policy on Write)

### 2.1 為什麼「寫檔時主動宣告」優於「執行期自動偵測遷移」？

| 比較維度 | 執行期偵測後動態遷移 (Delayed Migration) | 寫檔時主動宣告 (Explicit Policy) |
| :--- | :--- | :--- |
| **遷移成本** | 首次回退時需承擔高昂的「整檔解壓 $\to$ 切塊 $\to$ 逐塊重壓 $\to$ 重配區塊」延遲 | **零遷移成本**，從第 1 個 Byte 起即以 16KB/64KB 分區落盤 |
| **Domain Knowledge** | 檔案系統需盲猜存取模式，猜錯會造成反覆轉換開銷 | **應用層最清楚自身特性**（Log 走 Stream，DB 走 Seekable） |
| **實作複雜度** | 需處理並發寫入時的動態格式變換、鎖升級與 race condition | 格式在寫入時確定，狀態機純粹、易於驗證與 formal verification |

### 2.2 存取情境劃分

1. **情境 A（Append-Only / Write-Once）：日誌、封裝、靜態資料庫、Log/Archive**
   * 宣告為 `CompressionMode::Stream { level }`（或預設模式）。
   * 享有 100% 最高壓縮率（跨全檔 LZ77 滑動視窗）、Append 零索引負擔。
2. **情境 B（Mutable / Random Access）：資料庫、快取、編輯中的檔案、隨機索引**
   * 宣告為 `CompressionMode::Seekable { chunk_size: 16KB, level: 1 }`。
   * 兼顧壓縮節省空間，同時任意回退覆寫與 `read_at` 均在微秒級 ($O(Chunk)$) 完成。

---

## 3. 分區架構與磁碟佈局設計 (Chunked Layout & Seek Table)

### 3.1 邏輯切塊與獨立壓縮
檔案在邏輯上以固定大小（建議 16KB 或 64KB）切分為獨立 Chunk：
* `Chunk 0`: $[0 \dots 16\text{KB})$
* `Chunk 1`: $[16\text{KB} \dots 32\text{KB})$
* `Chunk 2`: $[32\text{KB} \dots 48\text{KB})$

每個 Chunk 經過獨立的 Zstd 壓縮，並依照 OIFS 原生的 4KB 區塊進行分配落盤：

```text
邏輯檔案 (16KB Chunks):
┌────────────────┬────────────────┬────────────────┐
│ Chunk 0 (16KB) │ Chunk 1 (16KB) │ Chunk 2 (16KB) │
└────────┬───────┴────────┬───────┴────────┬───────┘
         │ zstd(L1)       │ zstd(L1)       │ zstd(L1)
         ▼                ▼                ▼
實體磁區 (4KB Blocks):
┌────────┬───────┬────────┬───────┬────────┬───────┐
│ BlockA │ BlockB│ BlockC │ BlockD│ BlockE │  ...  │
└────────┴───────┴────────┴───────┴────────┴───────┘
```

### 3.2 局部 Read-Modify-Recompress
若應用層覆寫 `offset = 20KB, len = 5KB`（落在 `Chunk 1 [16KB..32KB)`）：
1. 僅需從磁碟讀取 `Chunk 1` 對應的實體區塊。
2. 解壓為 16KB 暫存緩衝區。
3. 覆寫其中的 5KB 資料。
4. 以 Zstd 重新壓縮該 16KB Chunk。
5. 將壓縮後的區塊寫回磁碟（釋放多餘區塊或按需增配）。
6. **完全不觸碰 Chunk 0 與 Chunk 2+**，寫入延遲由數百毫秒驟降至數十微秒。

---

## 4. API 規格設計

### 4.1 Rust API 設計

擴充 [`src/disk.rs`](file:///Users/ych/oifs/src/disk.rs) 中的 `CompressionMode`：

```rust
/// 壓縮策略與存取模式規格
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionMode {
    /// 不壓縮：以未壓縮 Raw 區塊儲存，支援 mmap 零拷貝極致讀寫
    Never,

    /// 全檔串流壓縮：適合純追加或一次性封裝，壓縮率最高
    Stream {
        /// Zstd 壓縮等級 (1 ~ 19，0 表示預設 Level 3)
        level: i32,
    },

    /// 可隨機回退讀寫之分區壓縮 (Seekable / Chunked)
    /// 支援高效任意 offset 覆寫與 read_at，每次僅解壓/重壓單一區段
    Seekable {
        /// 邏輯分區大小，建議 16384 (16KB) 或 65536 (64KB)
        chunk_size: u32,
        /// Zstd 壓縮等級 (推薦 Level 1 高吞吐)
        level: i32,
    },

    /// 自動判定（相容現行：>= 8KB 時自動採用串流壓縮）
    Auto,

    /// 強制壓縮（相容現行：採用預設等級之串流壓縮）
    Always,
}
```

寫入呼叫範例：
```rust
// 宣告該檔案支援回退，採用 16KB 分區與 Level 1 高速壓縮
dm.write_data_with_filters(
    inode_id,
    offset,
    &data,
    CompressionMode::Seekable { chunk_size: 16 * 1024, level: 1 },
    &filters,
)?;
```

---

### 4.2 C / C++ FFI API 設計 ([`include/oifs.h`](file:///Users/ych/oifs/include/oifs.h))

```c
/*
 * 寫入策略 Flags
 */
#define OIFS_WRITE_POLICY_DEFAULT       0x00  /* 自動 / 全檔串流壓縮 (現行預設行為) */
#define OIFS_WRITE_POLICY_RAW           0x01  /* 不壓縮 (極致 Raw 讀寫) */
#define OIFS_WRITE_POLICY_STREAM        0x02  /* 全檔串流壓縮 (可指定 zstd_level) */
#define OIFS_WRITE_POLICY_SEEKABLE_16K  0x04  /* 16KB 分區壓縮 (支援極速回退覆寫) */
#define OIFS_WRITE_POLICY_SEEKABLE_64K  0x08  /* 64KB 分區壓縮 (平衡型隨機讀寫) */

/**
 * 支援進階寫入策略與自訂 zstd level 的擴充寫入介面
 *
 * @param handle OIFS 檔案系統句柄
 * @param filename 檔案路徑
 * @param offset 寫入起始邏輯偏移量
 * @param buf 資料指標
 * @param buf_size 資料長度
 * @param policy_flags 策略旗標 (OIFS_WRITE_POLICY_*)
 * @param zstd_level 壓縮等級 (1 ~ 19，傳入 0 採用預設 Level 3)
 * @return 0 成功，-1 失敗並設置 oifs_last_error
 */
int32_t oifs_write_file_with_policy(
    OIFSHandle *handle,
    const char *filename,
    uint64_t offset,
    const uint8_t *buf,
    uint64_t buf_size,
    uint32_t policy_flags,
    int32_t zstd_level
);
```

#### C++20 風格封裝：
```cpp
// 搭配 C++20 std::span 進行零拷貝呼叫
std::span<const uint8_t> payload = get_buffer();
int ret = oifs_write_file_with_policy(
    handle,
    "database.db",
    offset,
    payload.data(),
    payload.size(),
    OIFS_WRITE_POLICY_SEEKABLE_16K,
    /*zstd_level=*/1
);
```

---

## 5. Zstd Level 效能基準與建議值

| 等級 | 壓縮吞吐量 | 解壓縮吞吐量 | 壓縮率表現 | 建議適用情境 |
| :--- | :--- | :--- | :--- | :--- |
| **Level 1** | **~800 MB/s+** | ~2.0 GB/s | 良好 (略低於 L3 5~10%) | **Seekable 16K/64K 回退寫入、即時資料庫** |
| **Level 3 (預設)** | ~350 MB/s | ~2.0 GB/s | 平衡基準 | 一般檔案、日誌記錄 |
| **Level 7 ~ 9** | < 80 MB/s | ~2.0 GB/s | 高 (再省 3~8%) | 冷資料歸檔、只讀資源包 |

> **關鍵架構原則**：Zstd 解壓縮速度與壓縮等級無關。調低壓縮等級至 Level 1 可使回退寫入重壓延遲減少 60% 以上，而讀取端不受任何影響。
