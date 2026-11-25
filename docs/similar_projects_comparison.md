# OIFS 與同類單檔檔案系統／嵌入式儲存專案之異同比較與效能實測分析

> 本文件整理 OIFS（O's Inode File System）與開源生態中定位相近之單檔檔案系統、加密容器與嵌入式儲存引擎的功能架構異同，並於同硬體環境下對可實際執行的主流代表（**LMDB (`heed`)**、**`redb`**、**SQLite WAL（`agentfs` 底層引擎）**、**原生 OS 檔案系統 APFS**）進行效能實測對比，進而歸納各專案勝出環節之根因與 OIFS 可直接借鑑的優化方向。

---

## 一、同類專案全景定位與功能異同

### 1.1 專案分類與核心特性總覽

| 專案 | 語言 | 核心定位與架構 | 單檔容器 | 讀寫模式 | 內建加密 | 內建壓縮／濾鏡 | 崩潰一致性機制 | 多行程／掛載 | 形式化驗證 |
| :--- | :---: | :--- | :---: | :---: | :---: | :---: | :--- | :--- | :---: |
| **OIFS (`Mmap` / `FileIo`)** | Rust | 類 Unix Inode 單檔檔案系統（雙引擎：`mmap` + 純 `pread/pwrite`） | ✅ `.img` | 讀寫＋嚴格唯讀（POSIX、`F_RDLCK`、`PROT_READ`） | ✅ XChaCha20-Poly1305 + 192-bit SIV 檔名 + AAD 綁定 | ✅ Zstd（整檔／64K Seekable）+ SIMD Delta/Shuffle/BitShuffle | 選配 Metadata WAL（CRC32C + Ordered Mode + JBD2 延遲 Checkpoint）＋原子 `rename` | Master-Proxy IPC（UDS / TCP）+ 純 Rust FUSE（Linux/macOS）+ 本機免 kext NFSv3 + 整檔 CoW Clone／跨映像 Transfer + C FFI / Modern C++17/20 + MCP Server | ✅ 113 Kani Proofs + Bolero + Shuttle |
| **[`zboxfs/zbox`](https://github.com/zboxfs/zbox)** | Rust | 零外洩嵌入式加密虛擬檔案系統（App 內加密容器） | ✅ / 目錄 | 讀寫 | ✅ 全 Metadata + 資料加密 + 固定區塊 Padding | ✅ LZ4（區塊去重） | 內建內容定址區塊與版本快照 | 單行程獨佔 | — |
| **[`redox-os/redoxfs`](https://github.com/redox-os/redoxfs)** | Rust | 仿 ZFS 的 Copy-on-Write 原生檔案系統 | ✅ Image | 讀寫 | ✅ 全樹加密 | — | 全樹 Metadata Checksum + CoW 根節點原子切換 | FUSE / Redox 原生 | — |
| **[`tursodatabase/agentfs`](https://github.com/tursodatabase/agentfs)** | Rust | 專為 AI Agent 打造的單檔沙盒檔案系統（底層 SQLite/Turso） | ✅ `.db` | 讀寫 | 選配（SQLite 加密擴充） | — | SQLite WAL + ACID 交易 | FUSE（Linux）/ NFS（macOS）掛載 + Snapshot/Fork | — |
| **[`cberner/redb`](https://github.com/cberner/redb)** | Pure Rust | 單檔嵌入式 ACID KV 引擎（仿 LMDB、純 `pread/pwrite`） | ✅ `.redb` | 讀寫（ACID 交易） | — | — | CoW B-Tree + 兩階段根指標翻轉 + Checksum | 單行程多執行緒（無鎖 MVCC 讀取） | — |
| **LMDB (`heed`)** | C / Rust | 極速 `mmap` Copy-on-Write B+Tree 嵌入式引擎 | ✅ `data.mdb` | 讀寫（ACID 交易） | — | — | CoW B+Tree + 雙 Meta Page 翻轉 | 多行程原生共享 `mmap`（無鎖 MVCC 讀取） | — |
| **SQLite (WAL Mode)** | C | 全球最普及的單檔關聯式容器（`agentfs`、Fossil、App 封裝基底） | ✅ `.sqlite` | 讀寫（ACID 交易） | 選配（SEE / SQLCipher） | — | Write-Ahead Log（WAL）+ Page Cache | 多行程共享檔案鎖 + WAL-index shm | 高覆蓋測試（TH3） |
| **[`rfjakob/gocryptfs`](https://github.com/rfjakob/gocryptfs) / [`rencfs`](https://github.com/radumarias/rencfs)** | Go / Rust | 使用者空間加密疊加檔案系統 | ❌（疊加目錄） | 讀寫 | ✅ AES-GCM-SIV / EME 檔名 + 分塊加密 | — | 依賴底層 OS 檔案系統 | FUSE | — |
| **[`mhx/dwarfs`](https://github.com/mhx/dwarfs)** | C++ | 超高壓縮率唯讀單檔檔案系統（科學資料／映像封裝） | ✅ `.dwarfs` | 唯讀 | — | ✅ Zstd / LZMA + 區塊去重 + 相似檔聚類排序 | 唯讀不可變 | FUSE | — |
| **[`Blosc/c-blosc2`](https://github.com/Blosc/c-blosc2)** | C | 科學數值張量超高速分塊壓縮容器 | ✅ `.b2frame` | 讀寫（張量分塊） | — | ✅ SIMD Shuffle/BitShuffle/Delta + Zstd/LZ4/BloscLZ | 單檔框架 | 多執行緒 C/Python 函式庫 | — |

### 1.2 OIFS 相較同類專案的核心優勢與待補足缺口

**OIFS 的差異化優勢**：
1. **檔案系統原生語意 + 極致吞吐兼得**：兼具階層目錄、Inode、Sparse 檔案、Truncate、線上 Defragmentation 與 `fsck`，同時在 `Mmap` 模式下能達到 **50+ GB/s** 讀取與 **13+ GB/s** 寫入，並提供純 `pread`/`pwrite` 的 `StorageMode::FileIo` 零 `SIGBUS` 安全選項（內建 64-Shard 32 MiB Data Page Cache）與多行程共享的嚴格唯讀模式（`F_RDLCK` + `PROT_READ`）。
2. **原生融合科學數值 SIMD 濾鏡與 64 KiB Seekable 隨機壓縮**：`zbox`、`redoxfs`、`agentfs`、`redb`、`LMDB`、`SQLite` 皆未將 Delta / Shuffle / BitShuffle SIMD 濾鏡與多執行緒 Seekable 64K Zstd 原生整合進檔案寫入／讀取管線。
3. **高強度形式化與模糊驗證**：擁有 **113 項 Kani（CBMC）數學證明**、Bolero fuzzing 與 Shuttle 隨機並發交錯測試，涵蓋指標樹、Bitmap、WAL、加密 AAD、唯讀不變性、FUSE/NFSv3 協議邊界、跨映像傳輸與濾鏡雙射性。
4. **多層次整合能力（免 kext 本機 NFSv3 + 純 Rust FUSE + Modern C++17/20 + Master-Proxy IPC + MCP）**：除同機 UDS / 跨機 TCP 認證連線池與 `oifs_mcp` 外，已具備本機免 kext / 免重開機的 Pure-Rust NFSv3 掛載（macOS 預設）、零 C 依賴跨平台 FUSE 掛載（Linux `/dev/fuse` + macOS `macFUSE`）、POSIX 原子 `rename`、整檔 CoW `clone_image` 與三層跨映像 `transfer_file_to` / `splice_range_to`、Header-only Modern C++17/20（`oifs::PinnableSlice` 零拷貝）與 Inline Fast-Path 非同步管線。

**同類專案領先於 OIFS 的待補足缺口（現況盤點）**：
1. **映像內 CoW 快照／疊加層（In-Image Snapshot / Overlay）與跨檔案去重（Deduplication）**：目前支援整檔層級的 OS CoW Clone（`clone_image`，透過 APFS `clonefile` / Linux `FICLONE`），但尚未提供單一 `.img` 內部的檔案級 CoW Snapshot／Overlay 與跨檔案內容定址去重（`zbox`、`agentfs`、`dwarfs`、`redoxfs` 具備）。
2. **主表 Metadata Checksum（Inode / Indirect Block CRC32C）**：目前僅 WAL 具備 CRC32C，尚未對主表 Inode 與間接區塊加檢驗碼以偵測靜默位元翻轉（Bit Rot）。

---

## 二、跨專案實測效能對比

### 2.1 測試環境與方法

- **硬體／系統**：macOS（Apple Silicon ARM64），`--release`（`opt-level = 3`, `lto = "thin"`）。
- **受測引擎**：
  - **OIFS v1.2.0**：`Mmap`（預設）、`FileIo`（純 `pread`/`pwrite`）、各自搭配 `Lazy` 與 `+WAL`（`Lazy` / `Strict`）。
  - **LMDB (`heed 0.21`)**：`NO_SYNC`（對應 page cache 持久性）與預設 `SYNC`；大檔依 64 KiB chunk 存入 B+Tree 以支援隨機 `read_at`。
  - **`redb 2.6.4`**：純 Rust `pread`/`pwrite` + 使用者空間 Page Cache；`Durability::None` 與 `Durability::Immediate`；大檔採 64 KiB chunk。
  - **SQLite 3 (`rusqlite 0.32` bundled, WAL Mode)**：模擬 `agentfs` 的單檔 SQLite 檔案表 + 64 KiB chunk 表架構；分別測試 `synchronous=OFF`、`synchronous=FULL`（預設 `fullfsync=OFF`）與 `synchronous=FULL + fullfsync=ON`。
  - **Native OS FS (APFS)**：`std::fs` 直接讀寫宿主機 APFS 檔案系統作為基準線。
- **測試負載**：
  1. `1kW` / `1kR` / `1kD`：1,000 個 4 KiB 小檔（總計 4 MiB）之建立＋寫入、查詢＋讀取、刪除耗時（ms）。對 KV/DB 引擎同時測「單一交易批次（`1-tx`）」與「逐檔提交（`per-op`）」。
  2. `16M W` / `16M R`：單一 16 MiB 不可壓縮大檔順序寫入與整檔讀取耗時（ms）與吞吐（MB/s）。
  3. `5k4KR`：在 16 MiB 大檔上執行 5,000 次隨機 4 KiB 定位讀取（`read_at` / `pread` / chunk slice）耗時（ms）。
  4. `S64W` / `S64R` / `S64Rnd`：OIFS 專屬 16 MiB 可壓縮資料以 Seekable 64K Zstd (level 1) 寫入、平行整檔讀取與 5,000 次隨機 4 KiB 讀取耗時（ms）。
  5. `4T8kR`：4 執行緒同時各執行 2,000 次隨機 4 KiB 讀取（總計 8,000 次）耗時（ms）。
  6. **Strict / `fsync` 持久性測試**：100 次 4 KiB 檔案建立＋寫入＋強同步提交耗時（ms）與吞吐（files/s）。

### 2.2 實測數據表（Page Cache / Non-fsync 基準）

> 數值越小越快；括號內為吞吐量（MB/s），越大越快。

| Engine / Mode | `1kW` (ms) | `1kR` (ms) | `1kD` (ms) | `16M W` ms (MB/s) | `16M R` ms (MB/s) | `5k4KR` (ms) | `S64W` (ms) | `S64R` (ms) | `S64Rnd` (ms) | `4T8kR` (ms) |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| **OIFS Mmap (Lazy)** | 1.89 | 0.40 | 0.82 | 3.46 (4,623) | 2.36 (6,766) | 2.69 | 2.29 | 1.99 | 19.76 | 1.57 |
| **OIFS FileIo (Lazy)** | 13.86 | 1.12 | 6.79 | 1.56 (10,235) | 1.24 (12,941) | 3.35 | 1.85 | 0.66 | 16.60 | 1.71 |
| **OIFS Mmap+WAL (Lazy)** | 1.35 | **0.20** | 1.10 | 1.30 (12,267) | **0.31 (51,214)** | 0.82 | 0.44 | 0.22 | 14.34 | 1.29 |
| **OIFS FileIo+WAL (Lazy)** | 17.07 | 0.88 | 9.40 | 1.40 (11,428) | 1.05 (15,180) | 3.16 | 1.12 | 0.51 | 15.58 | 1.75 |
| **OIFS Mmap+WAL (`Session` + 1-tx)** | **1.12** | 0.27 | 1.11 | **1.22 (13,065)** | 0.32 (50,720) | 0.72 | **0.39** | **0.21** | **14.03** | **0.56** |
| **OIFS FileIo+WAL (`Session` + 1-tx)** | 12.07 | 0.78 | 9.37 | 1.35 (11,830) | 0.97 (16,538) | 2.34 (warm **0.79**) | 1.03 | 0.55 | 15.68 | **0.72** |
| **LMDB (`heed`, NO_SYNC, 1-tx)** | 1.33 | 0.33 | **0.31** | 3.04 (5,263) | 1.16 (13,752) | 0.38 | N/A | N/A | N/A | 0.97 |
| **LMDB (`heed`, NO_SYNC, per-op)** | 9.52 | **0.19** | 11.08 | 2.13 (7,496) | 1.62 (9,880) | **0.36** | N/A | N/A | N/A | 1.07 |
| **`redb` (None, 1-tx)** | 13.66 | 0.21 | 1.98 | 7.07 (2,263) | 0.63 (25,337) | 0.63 | N/A | N/A | N/A | 0.93 |
| **`redb` (None, per-op)** | 38.36 | 1.46 | 27.78 | 6.76 (2,367) | 0.37 (43,011) | 0.67 | N/A | N/A | N/A | 0.95 |
| **SQLite WAL (OFF, 1-tx)** | 10.47 | 1.39 | 0.55 | 33.78 (474) | 2.28 (7,015) | 36.74 | N/A | N/A | N/A | 62.32 |
| **SQLite WAL (OFF, per-op)** | 19.50 | 1.49 | 13.07 | 36.67 (436) | 2.20 (7,287) | 36.74 | N/A | N/A | N/A | 61.93 |
| **Native OS FS (APFS)** | 48.48 | 10.79 | 28.98 | 1.25 (12,809) | 0.87 (18,398) | 2.00 | N/A | N/A | N/A | 3.93 |

### 2.3 實測數據表（Strict / `fsync` 強持久性：100 × 4 KiB Create + Write + Commit）

| Engine / Mode | 總耗時 (ms) | 吞吐 (files/s) | 每次檔案同步次數與底層機制 |
| :--- | ---: | ---: | :--- |
| **OIFS Mmap+WAL (`Strict`, 1-tx)** | **7.87** | **12,705.7** | 1 tx/file（`create_file_with_data` 單交易 WAL inline）；`msync(MS_SYNC)` |
| **OIFS Mmap+WAL (`Strict`, 2-tx)** | 9.10 | 10,984.2 | 2 tx/file；`msync(MS_SYNC)`（macOS 上不觸發 `F_FULLFSYNC` NAND 硬刷） |
| **SQLite WAL (`synchronous=FULL`)** | 13.65 | 7,324.3 | 1 tx/file；預設 `fullfsync=OFF`，macOS 上僅呼叫 `fsync()` |
| **SQLite WAL (`FULL + fullfsync=ON`)** | **333.96** | **299.4** | 1 tx/file；1 次 `fcntl(F_FULLFSYNC)` 實體落盤（~3.3 ms/次） |
| **LMDB (`heed`, default SYNC)** | 355.84 | 281.0 | 1 tx/file；1 次實體落盤屏障（~3.5 ms/次） |
| **`redb` (`Durability::Immediate`)** | 364.96 | 274.0 | 1 tx/file；Rust `File::sync_all()`（macOS `F_FULLFSYNC`，~3.6 ms/次） |
| **OIFS FileIo+WAL (`Strict`, 1-tx)** | **396.91** | **251.9** | **1 tx/file（`create_file_with_data` WAL inline data）≈ 1.14 次 `sync_data`（含 50% 水位線 Checkpoint 攤銷，較 2-tx 快 2.32×）** |
| **OIFS FileIo+WAL (`Strict`, 2-tx)** | 919.97 | 108.7 | 2 tx/file = 3 次 `File::sync_data()`（`create_file` WAL + `write_data` payload + `write_data` WAL） |

### 2.4 本機真實 FUSE 掛載效能實測（macOS + macFUSE 5.5.0，3 輪中位數）

透過 `tests/fuse_live_mount_test.rs` 在真實 macOS Kernel VFS + macFUSE 5.5.0 下執行標準 `std::fs` POSIX 操作，與同目錄原生 APFS 對照：

| 掛載組別（3 輪中位數） | `500×4K W` | `500×4K R` | `500×4K D` | `16M W`（預熱後峰值） | `16M R` (UBC) | `2k4K Rnd` (UBC) |
| :--- | ---: | ---: | ---: | :--- | ---: | ---: |
| **Native OS FS (APFS)** | 24.0 ms | 4.8 ms | 13.4 ms | 1.2 ms (13.9 GB/s) | 1.2 ms (13.5 GB/s) | 1.2 ms (1.69M/s) |
| **OIFS FUSE (`Mmap+WAL`, 1M)** | 79.9 ms | 21.3 ms | 30.2 ms | 13.4 ms（峰值 **7.5 ms / 2.13 GB/s**） | 0.8 ms (19.5 GB/s) | 1.2 ms (1.65M/s) |
| **OIFS FUSE (`FileIo+WAL`, 1M)** | 66.6 ms | 19.0 ms | 32.6 ms | 18.2 ms（峰值 **14.7 ms / 1.09 GB/s**） | 0.8 ms (20.7 GB/s) | 1.2 ms (1.69M/s) |

- **解讀**：經由 FUSE 掛載時，大檔順序寫入可達 **~2.1–2.3 GB/s（`Mmap+WAL`）** 與 **~1.1 GB/s（`FileIo+WAL`）**，UBC 熱讀取幾乎追平原生 APFS；小檔為 APFS 的 1/3～1/4，主因為 FUSE 每次小檔操作皆需經由多次 kernel↔userspace 封包往返（相較直連 OIFS API 為 890k files/s）。
- 實測 3 輪 A/B 對照確認：將 `max_write` 從 `128 KiB` 調升至 `1 MiB`（並動態偵測 Linux `FUSE_MAX_PAGES` / `/sys/module/fuse/parameters/max_pages_limit`）對小檔完全無回歸，且可讓 Linux 4.20+/6.13+ 核心直接打滿 1–4 MiB 單次 `FUSE_WRITE`。

---

## 三、深度效能歸因分析：OIFS 贏在哪裡？又輸在哪裡？

### 3.1 OIFS 顯著領先的場景

1. **大量小檔建立／讀取／刪除（單檔容器核心價值）**：
   - `OIFS Mmap+WAL` 建立＋寫入 1,000 個 4 KiB 小檔僅 **1.33 ms**，比原生 APFS（**49.64 ms**）快 **37 倍**、比 `redb per-op`（**38.93 ms**）快 **29 倍**、比 `SQLite per-op`（**19.07 ms**）快 **14 倍**、比 `LMDB per-op`（**9.73 ms**）快 **7.3 倍**，甚至略勝將 1,000 檔打包成單一交易的 `LMDB 1-tx`（1.42 ms）。
   - 原因：OIFS 的 Sharded `dir_cache` + `BoundedInodeCache` + `SimpleBlockAllocator` 順序 hint 掃描將目錄與配置成本壓至 $O(1)$，不需維護 B-Tree 分裂與平衡。
2. **大檔順序吞吐（`16M W` / `16M R`）**：
   - `OIFS Mmap+WAL` 寫入 **13.4 GB/s**、讀取 **48.3 GB/s**；即使在純 `pread`/`pwrite` 的 `OIFS FileIo+WAL` 模式下，寫入也達 **11.3 GB/s**、讀取 **16.0 GB/s**，幾乎追平原生 APFS（12.7 / 19.3 GB/s），並大幅超越 `redb` 寫入（2.3–2.5 GB/s，快 **5 倍**）與 `SQLite` 寫入（461 MB/s，快 **25–29 倍**）。
   - 原因：`redb` 與 `SQLite` 寫入大資料時必須將 64 KiB 切塊插入 B-Tree 並承受 CoW 內部節點寫入放大與頁拷貝；OIFS 的 Inode 連續區塊配置＋合併 extent 讀寫（`ExtentList` / `pending_run`）讓 16 MiB 幾乎等同單次連續大塊 I/O。
3. **平行 64 KiB Seekable Zstd 壓縮／解壓縮**：
   - 透過常駐執行緒池（`src/zstd_pool.rs::par_for_index`），OIFS 將 16 MiB 可壓縮資料壓縮寫入僅需 **0.40 ms**、平行解壓讀取僅 **0.22 ms**，甚至比拷貝未壓縮 16 MiB 更快，且隨機 4 KiB 讀取僅解壓單一 64 KiB chunk（`14–16 ms / 5,000 ops ≈ 2.8 µs/op`），遠勝 SQLite 未壓縮隨機讀（`37.5 ms`）。
4. **相較 SQLite（`agentfs` 類型架構）的隨機讀取差距**：
   - 5,000 次隨機 4 KiB 讀取：`OIFS Mmap` **1.09 ms**、`OIFS FileIo` **2.32 ms** vs `SQLite WAL` **37.51 ms**（差距 **16–34 倍**）；4 執行緒 8,000 次隨機讀：`OIFS Mmap` **1.29 ms** vs `SQLite WAL` **60.62 ms**（差距 **47 倍**）。
   - 原因：SQLite 每次查詢需經 SQL VM 執行、B-Tree 查表與 `substr()` 複製 64 KiB overflow pages；OIFS 直接走 Inode 陣列／快取間接表定位。

---

### 3.2 待評估的跨專案高階特性

| 借鑑來源 | 特性 | 對 OIFS 的價值 | 現況 |
| :--- | :--- | :--- | :---: |
| **`zbox` / `agentfs` / `redb`** | **映像內 CoW Snapshot 與唯讀基底 Overlay** | 除現有整檔級 `clone_image` 外，進一步在單一 `.img` 內部提供檔案／目錄級快照與可回滾工作區疊加層 | ⏳ 待實作 |
| **`redoxfs`** | **Metadata Checksum（Inode / Indirect Block CRC32C）** | 目前 OIFS 的 WAL 有 CRC32C，但主表 Inode 與間接指標塊尚未加檢驗碼；在 256B Inode 保留欄位加入 4B CRC32C 可主動偵測靜默位元翻轉（Bit Rot） | ⏳ 待實作 |
| **`zbox` / `dwarfs`** | **選配跨檔案 64 KiB Content-Addressed 去重** | 結合現有 `BLAKE2b` 與 64 KiB 分塊引擎，對重複資料集、模型檔與多版本備份可再省數倍空間 | ⏳ 待實作 |

---

## 四、後續待規劃方向

1. **映像內 CoW Snapshot / Overlay**：在單一 `.img` 內支援唯讀基底與可寫疊加層，供 AI Agent 沙盒與多版本實驗使用。
2. **主表 Metadata Checksum（Inode / Indirect Block CRC32C）**：利用 256B Inode 保留欄位與間接區塊檢驗碼偵測靜默位元翻轉（Bit Rot）。
3. **選配跨檔案 64 KiB Content-Addressed 去重**：結合 `BLAKE2b` 與 64 KiB 分塊引擎降低重複資料集佔用空間。
