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

### 3.2 回退寫入與崩潰一致性策略：策略 B (Copy-on-Write / COW Extent Swap)

在回退修改（如覆寫 64KB Chunk 中的某個片段）時，重壓後的資料大小往往會產生浮動（例如從 44KB 變為 48KB，或縮小為 36KB）。OIFS 嚴格採用**策略 B（寫入時複製，Copy-on-Write）**保障資料安全與極致的崩潰一致性 (Crash Consistency)：

```text
舊區塊: [ Blk 101 ~ 111 (11 blocks / 44KB) ] ─── 保持原樣不動 (安全防護屏障)
                                                      
新區塊: [ Blk 201 ~ 212 (12 blocks / 48KB) ] ─── 在磁碟空閒處直接寫入新資料
            │
            ▼ 步驟 3: 寫入完成後原子切換
Inode 指標原子替換指向 [ Blk 201 ~ 212 ]
舊的 [ Blk 101 ~ 111 ] 釋放回 Data Bitmap (步驟 4)
```

#### 具體操作流程：
1. **讀取與局部重壓**：
   - 僅從磁碟讀出該目標 Chunk 對應的舊實體區塊（如 11 個 blocks）。
   - 解壓為暫存緩衝區，覆寫應用層指定的偏移片段。
   - 以 Zstd (Level 1) 重新壓縮該 Chunk，計算新實體需求：$N_{new} = \lceil \text{new\_comp\_size} / 4096 \rceil$（如 $48\text{KB} \to 12$ 個區塊）。
2. **全新空間配置 (COW Allocate)**：
   - 從 `data_bitmap` 申請配置全新 $N_{new}$ 個實體區塊（如 12 個 blocks），**完全不覆蓋舊區塊**。
3. **無鎖/安全寫入**：
   - 將新壓縮資料寫入新配置的區塊中。
4. **原子指標切換 (Atomic Pointer Swap)**：
   - 更新 Inode Direct / Indirect 指標樹，將該 Chunk 的映射指向全新區塊集合，並更新 `compressed_size`。
   - 若啟用了 Metadata WAL Journal，指標切換會作為一筆原子交易提交。
5. **舊區塊回收 (Reclaim)**：
   - 指標切換確認持久化後，將舊有的 $N_{old}$ 個區塊（如 11 個 blocks）歸還給 `data_bitmap`。

#### 崩潰安全優勢：
* **零損壞視窗**：若在資料壓縮、新區塊寫入的任何瞬間發生斷電或系統當機，Inode 指標依然指向舊區塊，重開機後舊資料 100% 完好無損，絕不會出現「寫了一半的半殘 Chunk」。
* **極端膨脹防禦 (Anti-Inflation Fallback)**：
  若該 Chunk 修改後寫入高熵資料（如隨機數或加密流），導致壓縮後 $N_{new} \ge N_{raw}$（例如 64KB 壓縮後 $> 64\text{KB}$），系統自動放棄壓縮，改為分配 16 個 Raw blocks 以未壓縮形式寫入，並標記該 Chunk 為 Raw，防止負壓縮效益。

### 3.3 連續區塊 Metadata 注記：方案二 (ChunkExtent) 規格與潛在挑戰分析

#### 1. ChunkExtent 結構體定義 (8 Bytes 緊湊佈局)
```rust
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkExtent {
    /// 實體連續起始區塊 ID (4KB 區塊，支援高達 16 TB 單一映像檔)
    pub start_block: u32,
    /// 佔用的連續實體區塊個數 (例如 11 個 4KB 區塊)
    pub block_count: u16,
    /// 實際壓縮後的精確位元組長度 (例如 45,056 bytes，供 Zstd 及 AEAD 解密安全校驗)
    pub compressed_len: u16,
}
```

#### 2. 方案二的潛在問題與對應防禦手段 (Critical Challenges & Mitigations)

##### 問題一：磁碟高度碎片化時，無法分配到「連續區塊」 (Contiguous Allocation Failure)
* **現象**：當硬碟剩餘空間低且極度零碎時，寫入一個 48KB Chunk（需要 12 個區塊），磁碟可能無法提供連續的 12 個 blocks，僅有零散的區塊可用。
* **因應手段 (三層防護)**：
  1. **首選配置**：分配器優先嘗試在同一 Extent 連續區間內申請空間。
  2. **自動連鎖 Defrag**：若連續空間不足但總空閒空間充足，觸發內部輕量 In-Place Defrag Compaction，立即騰出連續空間。
  3. **Fallback 降級機制**：若仍無法連續，允許將該 Chunk 退化為未壓縮 Raw 區塊（直接沿用既有 Direct/Indirect 個別指標鏈），確保寫入永遠 100% 成功不報錯。

##### 問題二：64KB 上限與 16-bit 邊界問題 (64KB Boundary Overflow)
* **現象**：`u16` 的最大值為 65,535。如果 Chunk 大小剛好是 64KB (65,536 bytes)，未壓縮或壓縮不良時長度剛好超出 `u16` 範圍 1 byte。
* **因應手段**：
  * 定義 `compressed_len = 0` 特別代表「剛好 65,536 bytes（64KB）」；或者：
  * 當壓縮長度 $\ge 65,536$ 時，已失去壓縮效益，觸發 Anti-Inflation 直接改以 Raw 模式儲存。

##### 問題三：跨 Chunk 邊界寫入的交易原子性 (Cross-Chunk Write Atomicity)
* **現象**：若寫入請求跨越了 64KB 邊界（例如 offset = 60KB, len = 8KB，橫跨 Chunk 0 與 Chunk 1）。
* **因應手段**：
  * 系統需拆解為針對 Chunk 0 與 Chunk 1 的兩筆獨立 COW 操作。
  * 必須透過現有的 **Metadata WAL Journal** 將這兩個 ChunkExtent 的指針交換包裝在同一筆原子交易中，確保「要麼兩個 Chunk 同時更新成功，要麼同時回滾」，杜絕只更新一半的中間損毀狀態。

##### 問題四：非整除尾端 Chunk (Tail Chunk Truncation)
* **現象**：檔案大小非 64KB 整數倍（如 70KB，Chunk 0 為 64KB，Chunk 1 僅 6KB）。
* **因應手段**：
  * Inode 原生記錄了 `inode.size`（整體精確邏輯大小）。
  * 尾端 Chunk 解壓後，系統直接依據 `inode.size % chunk_size` 自動裁切有效資料，乾淨透明。

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

---

## 6. 待實作項目整體相依性與優先級拓撲 (Dependency Graph & Roadmap)

### 6.1 模組相依性分析

七大待實作項目並非完全互相獨立，其底層資料結構與執行路徑存在明確的層級關係：

```text
┌────────────────────────────────────────────────────────┐
│  項目 1: 連續區塊分配器 (P4.4) (難度: ⭐⭐, 重要性: ⭐⭐⭐⭐) │
│  - 於 src/bitmap.rs 提供 find_contiguous_free(count)   │
└──────────────────────────┬─────────────────────────────┘
                           │ 強相依 (提供連續 Extent)
                           ▼
┌────────────────────────────────────────────────────────┐
│  項目 2: 可回退分區壓縮 (難度: ⭐⭐⭐⭐, 重要性: ⭐⭐⭐⭐⭐)   │
│  - 採用方案二 ChunkExtent (8-byte: start_block + count)│
│  - 採用策略 B Copy-on-Write Extent Swap                │
└────────────┬─────────────────────────────┬─────────────┘
             │ 協同效應                    │ 驗證伴隨
             ▼                             ▼
┌───────────────────────────┐ ┌──────────────────────────┐
│ 項目 4: In-Place 解密與   │ │ 項目 6: Kani 形式化驗證  │
│         Filter 管道 (P4.9)│ │         擴展             │
└───────────────────────────┘ └──────────────────────────┘

[獨立分支項目]:
* 項目 3: 未壓縮 Raw 寫入截斷語意 (Truncate) ──── 完全獨立 (優先修復)
* 項目 5: IPC Buffer Pooling 與邊界防禦 (P4.8) ─── 完全獨立 (IPC 層)
* 項目 7: 背景自動 Defrag 協調器 ──────────────── 運維輔助 (減少項目 1 & 2 碎片)
```

1. **項目 1 (連續分配器) $\longrightarrow$ 項目 2 (可回退分區壓縮) 【強相依 / 關鍵前置】**：
   * 方案二的核心結構 `ChunkExtent` 是以 `(start_block, block_count)` 表示一個壓縮後的 Chunk。
   * 若無連續區塊分配器，項目 2 每次寫入都無法保證取得連續 Block，會頻繁引發 Fallback 降級。
   * **結論**：先做項目 1（提供連續分配能力），項目 2 即可直接呼叫並無縫串接。

2. **項目 7 (背景自動 Defrag) $\cdots\cdots$ 項目 1 / 項目 2 【運維輔助 / 協同關係】**：
   * 當磁碟嚴重碎片化時，項目 1 可能因無法滿足連續性而失敗。
   * 項目 7 可在背景空閒時壓實磁區、製造連續區間，提升項目 1 與 2 的連續配置命中率。
   * 但項目 2 不強制等待項目 7，因為項目 2 本身具有 Fallback 降級回 Raw 指標鏈的保底機制。

3. **項目 2 (分區壓縮) $\cdots\cdots$ 項目 4 (In-Place Filter & 解密管道) 【弱相依 / 讀取協同】**：
   * 項目 4 消除 `read_data_internal` 的中繼記憶體分配。
   * 當項目 2 實作 Chunked 讀取後，項目 4 的 In-Place 機制可直接作用於單一 Chunk 緩衝區，使隨機讀取的記憶體配置降至零。

4. **項目 3 (截斷語意) 與 項目 5 (IPC 防禦) 【完全獨立】**：
   * 項目 3 僅修改 Inode 大小與尾端區塊釋放，不影響分區與分配器。
   * 項目 5 位於 `src/ipc.rs` 進程間通訊層，與底層儲存引擎完全解耦。

5. **項目 6 (Kani 形式化驗證) 【驗證伴隨】**：
   * 形式化證明應伴隨項目 1（證明連續分配區間不越界、不重疊）與項目 2（證明 Extent 交換一致性）共同推進，避免先寫舊證明而後續需重構。

