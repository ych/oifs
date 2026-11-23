# OIFS 與同類單檔檔案系統／嵌入式儲存專案之異同比較與效能實測分析

> 本文件整理 OIFS（O's Inode File System）與開源生態中定位相近之單檔檔案系統、加密容器與嵌入式儲存引擎的功能架構異同，並於同硬體環境下對可實際執行的主流代表（**LMDB (`heed`)**、**`redb`**、**SQLite WAL（`agentfs` 底層引擎）**、**原生 OS 檔案系統 APFS**）進行效能實測對比，進而歸納各專案勝出環節之根因與 OIFS 可直接借鑑的優化方向。

---

## 一、同類專案全景定位與功能異同

### 1.1 專案分類與核心特性總覽

| 專案 | 語言 | 核心定位與架構 | 單檔容器 | 讀寫模式 | 內建加密 | 內建壓縮／濾鏡 | 崩潰一致性機制 | 多行程／掛載 | 形式化驗證 |
| :--- | :---: | :--- | :---: | :---: | :---: | :---: | :--- | :--- | :---: |
| **OIFS (`Mmap` / `FileIo`)** | Rust | 類 Unix Inode 單檔檔案系統（雙引擎：`mmap` + 純 `pread/pwrite`） | ✅ `.img` | 讀寫＋嚴格唯讀（POSIX、`F_RDLCK`、`PROT_READ`） | ✅ XChaCha20-Poly1305 + 192-bit SIV 檔名 + AAD 綁定 | ✅ Zstd（整檔／64K Seekable）+ SIMD Delta/Shuffle/BitShuffle | 選配 Metadata WAL（CRC32C + Ordered Mode + JBD2 延遲 Checkpoint） | Master-Proxy IPC（UDS / TCP）+ 純 Rust FUSE（Linux/macOS）+ C FFI / Modern C++17/20 + MCP Server | ✅ 102 Kani Proofs + Bolero + Shuttle |
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
3. **高強度形式化與模糊驗證**：擁有 **102 項 Kani（CBMC）數學證明**、Bolero fuzzing 與 Shuttle 隨機並發交錯測試，涵蓋指標樹、Bitmap、WAL、加密 AAD、唯讀不變性、FUSE 協議邊界與濾鏡雙射性。
4. **多層次整合能力（純 Rust FUSE + Modern C++17/20 + Master-Proxy IPC + MCP）**：除同機 UDS / 跨機 TCP 認證連線池與 `oifs_mcp` 外，已具備零 C 依賴的跨平台 FUSE 掛載引擎（Linux `/dev/fuse` + macOS `macFUSE`，單一 `.img` OS FD 複用千個開檔控制代碼）、Header-only Modern C++17/20（`oifs::PinnableSlice` 零拷貝）與 Inline Fast-Path 非同步管線。

**同類專案領先於 OIFS 的能力缺口（現況盤點）**：
1. **免 kext / 免重開機的本機 NFSv3 掛載**：Linux/macOS FUSE 已落地並通過實機驗證，但 macOS 仍需安裝並授權 macFUSE 系統延伸功能；下一步規劃內建 Pure-Rust 本機 NFSv3 Server（詳見 `docs/nfs_mount_design.md`）以達成 macOS 開箱即掛。
2. **POSIX 原子 `rename`（與按需的有界批次提交）**：單檔單交易 `create_file_with_data` 與非同步 Group-Commit 已落地；尚未支援跨目錄原子 `rename`，至於完整資料庫級 `WriteTransaction` 因 128 KiB WAL ring 與檔案系統定位，現階段評估 ROI 較低、不列優先。
3. **跨檔案去重（Deduplication）與 CoW 快照／分支（Snapshot / Fork）**（`zbox`、`agentfs`、`dwarfs`、`redoxfs` 具備）。
4. **主表 Metadata Checksum（Inode / Indirect Block CRC32C）**：目前僅 WAL 具備 CRC32C，尚未對主表 Inode 與間接區塊加檢驗碼以偵測靜默位元翻轉（Bit Rot）。

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

### 2.4 優化前後實測對比（已落地 P0 + P1 + P2）

| 關鍵指標 | 優化前 (Baseline) | 優化後 (P0+P1+P2) | 加速倍數 / 變化 |
| :--- | ---: | ---: | :--- |
| **`OIFS FileIo (Lazy)` 16 MiB 順序寫 (`16M W`)** | 11.96 ms (1,338 MB/s) | **1.56 ms (10,235 MB/s)** | **7.7× 加速**（單獨測試達 12.7 GB/s） |
| **`OIFS FileIo+WAL (Strict)` 100×4 KiB 建檔寫入** | 914.94 ms (109.3 files/s) | **396.91 ms (251.9 files/s)** | **2.31× 加速**（逼近 `redb` 365 ms / `LMDB` 356 ms） |
| **`OIFS FileIo+WAL` 4 執行緒 8,000 次隨機 4K 讀 (`4T8kR`)** | 4.54 ms | **0.72 ms** (`Session`+Cache) | **6.3× 加速**（反超 `redb` 0.93 ms 與 `LMDB` 0.97 ms） |
| **`OIFS Mmap+WAL` 4 執行緒 8,000 次隨機 4K 讀 (`4T8kR`)** | 1.29 ms | **0.56 ms** (`Session`) | **2.3× 加速**（全場第 1，快過 `redb`/`LMDB` 1.7×） |
| **`OIFS FileIo+WAL` 熱快取 5,000 次隨機 4K 讀 (`5k4KR`)** | 2.32 ms | **0.79 ms** (warm) | **2.9× 加速**（逼近 `redb` 0.63 ms） |
| **`OIFS Mmap+WAL` 1,000 小檔建立＋寫入 (`1kW`)** | 1.33 ms | **1.12 ms** (`create_file_with_data`) | **全場第 1**（快過 `LMDB 1-tx` 1.33 ms、快過 APFS **43×**） |
| **`OIFS FileIo+WAL` 1,000 小檔建立＋寫入 (`1kW`)** | 19.04 ms | **12.07 ms** (`create_file_with_data`) | **1.58× 加速**（-37% 耗時，快過 `redb 1-tx` 13.66 ms） |

### 2.5 本機真實 FUSE 掛載效能實測（macOS + macFUSE 5.5.0，3 輪中位數）

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

### 3.2 其他專案勝過 OIFS 的 5 個關鍵環節與可借鑑設計

這正是本次實測最有價值的發現——以下 5 個項目中，競品明顯領先 OIFS，且每一項都有清晰、可落地的架構改進方向：

#### 借鑑 1：隨機小塊讀取與多核擴展（`LMDB` 0.35 ms / `redb` 0.64 ms vs `OIFS Mmap` 1.09 ms / `OIFS FileIo` 2.32 ms）

- **現象**：
  - 單執行緒 5,000 次隨機 4 KiB 讀取：`LMDB` **0.35 ms**、`redb`（純 `pread` 無 `mmap`！）**0.64 ms**，竟然比 `OIFS Mmap`（**1.09 ms**）更快，且比 `OIFS FileIo`（**2.32 ms**）快 **3.6 倍**！
  - 4 執行緒 8,000 次隨機 4 KiB 讀取：`redb` **0.91 ms**、`LMDB` **1.03 ms** vs `OIFS Mmap` **1.29 ms**、`OIFS FileIo` **4.54 ms**。
- **根因分析**：
  1. **`redb` 擁有涵蓋 Data Pages 的使用者空間 Page Cache**（預設 LRU/Clock 快取）：熱點 4 KiB／64 KiB 資料直接命中 RAM，完全省去 `pread()` syscall（macOS 單次 `pread` 約 0.4–0.5 µs，5,000 次就是 ~2.2 ms，剛好解釋 `OIFS FileIo` 的 2.32 ms！）。而 `OIFS FileIo` 目前僅快取 Metadata 區塊，每次 Data `read_at` 都進核心呼叫 `pread`。
  2. **MVCC ReadTransaction 攤銷鎖與查表開銷**：`LMDB` 與 `redb` 在一次 `begin_read()` 快照交易內連續讀取 5,000 次，期間 **0 次 `RwLock` 原子計數器爭用**、**0 次 Inode 重複查找**；而 OIFS 每次 `read_at(inode_id, ...)` 都要進出 `Arc<RwLock<DiskManagerInner>>::read()`（4 核心同時對同一個 atomic reader count 做 `fetch_add`/`fetch_sub` 造成 cache-line bouncing！）並重複查詢 `BoundedInodeCache`。
- **可借鑑改進**：
  - **短期（低風險、高收益）**：提供 `DiskManager::open_read_guard()` / `ReadSession`（在單一 `RwLockReadGuard` 生命週期內批次執行多次 `read_at` 與快取 `Inode`），消除高頻隨機讀的鎖與查表開銷。
  - **中期**：為 `StorageMode::FileIo` 加入**選配的有界使用者空間 Data Page Cache**（例如 16–64 MiB 分片 Clock/LRU cache，寫入／截斷時精準失效對應 `block_id`），讓 `FileIo` 的熱點隨機讀直接追平甚至超越 `redb`（從 2.32 ms 降至 ~0.5 ms）。

#### 借鑑 2：`OIFS FileIo` 非 Journaled 路徑的大檔寫入落差（`FileIo Legacy` 11.96 ms vs `FileIo+WAL` 1.42 ms — 慢了 8.4 倍！）

- **現象**：
  - `OIFS FileIo+WAL` 寫 16 MiB 只需 **1.42 ms（11.3 GB/s）**，但非 Journaled 的 `OIFS FileIo (Lazy)` 卻要 **11.96 ms（1.34 GB/s）**！
- **根因分析**：
  - Journaled 路徑透過 `AllocSim` 在記憶體模擬所有 4,096 個區塊的配置，payload 透過 `pending_run` 合併為 **1 次 16 MiB `pwrite`**，最後 commit 時整批套用 1 次 `SetDataBitmapRange`（1 次 bitmap `pwrite`）。
  - 非 Journaled 路徑的 `write_buffer_at_offset` 迴圈內逐塊呼叫 `allocate_block_unzeroed()`，每次離開區塊就 drop `BitmapSliceMut` 觸發整張 4 KiB Data Bitmap 的 `pwrite`，同時每塊呼叫一次 8-byte `write_block_ptr()` `pwrite`——導致 16 MiB 寫入額外多出 **8,192 次小 `pwrite` syscall**！
- **可借鑑改進**：
  - 在 `write_buffer_at_offset` 的連續新區塊配置路徑中，改用 `allocate_contiguous_blocks_internal`（或批次持有 `data_bitmap_slice_mut` 與間接指標區塊的整塊緩衝，填完一個 512-entry 間接塊才 `pwrite` 一次）。預估可將非 Journaled `FileIo` 的 16 MiB 寫入從 **11.96 ms 直接壓到 ~1.3 ms（~9 倍加速）**。

#### 借鑑 3：`Strict` 持久性下「單檔建立＋寫入」的 `fsync` 放大（`OIFS FileIo+WAL Strict` 915 ms vs `SQLite fullfsync` 322 ms / `redb` 356 ms / `LMDB` 353 ms）

- **現象**：
  - 在真正的實體落盤屏障（macOS `F_FULLFSYNC`）下，100 個檔案建立＋寫入：`SQLite fullfsync` **322 ms**、`LMDB` **353 ms**、`redb` **356 ms**，而 `OIFS FileIo+WAL Strict` 為 **915 ms**（約慢 2.6–2.8 倍）。
- **根因分析**：
  - `redb` / `LMDB` / `SQLite` 將「建立檔名＋寫入資料」合併在**同一筆交易（1 次 `F_FULLFSYNC` ≈ 3.2–3.5 ms）**。
  - OIFS 呼叫端分兩步：`create_file()`（1 次 WAL `sync_data`）＋ `write_data()`（Ordered Mode 先 `sync_data` payload，再 `sync_data` WAL）= **每檔 3 次 `F_FULLFSYNC`（3 × 3.05 ms ≈ 9.15 ms）**！
- **可借鑑改進**：
  1. **新增單交易複合 API `create_file_with_data(parent, name, data, compression_mode)`**：把 Inode 配置、目錄項寫入、Payload 落盤與 Metadata WAL 合併為 1 筆交易（從 3 次 `sync_data` 降為 2 次；若小檔 `<= 4 KiB` 改走 WAL inline data 或 data-journaling，更可降到 **1 次 `sync_data` ≈ 320 ms**，直接追平 SQLite / redb / LMDB！）。
  2. **可選的 Group Commit / 批次交易 API（`dm.batch_write(|tx| { ... })`）**：借鑑 `redb` / `LMDB` / `SQLite` 的 `1-tx` 批次模式，讓批次匯入 1,000 個小檔只觸發 **1 次 WAL commit + 1 次 `sync_data`**，讓 `FileIo` 的 `1kW` 從 19 ms 降到 ~2 ms、`Strict` 批次匯入提升 **100 倍**。

#### 借鑑 4：非 `mmap` 批次寫入緩衝（`FileIo` 1,000 小檔 `14.24 ms` / `19.04 ms` vs `LMDB 1-tx` `1.42 ms` / `OIFS Mmap` `1.33 ms`）

- **現象**：
  - `OIFS FileIo` 單檔小操作每次都立即 `pwrite`（Inode Bitmap、Inode Slot、Dir Block、Parent Inode、Data Bitmap、Data Block、WAL Region），1,000 個小檔產生超過 8,000 次獨立 `pwrite` syscall。
- **根因與借鑑**：
  - `redb` 在 `WriteTransaction` 內將所有髒頁留在 User-Space Dirty Page Cache，直到 `commit()` 才合併排序後批次寫出；`SQLite` 的 Pager 亦同。
  - OIFS 在 `FileBacking` 的 `JournalRegionMut::drop` 目前每次 drop 都將整個 132 KiB `journal_buf` 寫回磁碟！其實每次 WAL `append` 只改了「4 KiB header 頭＋剛寫入的那一段 frame（通常僅數百 bytes）」。
  - **立即可做的微小優化**：讓 `JournalRegionMut` 只 `pwrite` 實際變動的 header（64 bytes）與剛寫入的 ring 區間（`ring.last_write()`），而非每次 `commit_journal_tx` 都 `pwrite` 整整 **132 KiB**！這單獨一項就能把 `OIFS FileIo+WAL` 的小檔寫入／刪除（`1kW` 19.04 ms / `1kD` 11.15 ms）大幅減半以上！

#### 借鑑 5：跨專案高階特性（非單純微效能，但決定生態競爭力）

| 借鑑來源 | 特性 | 對 OIFS 的價值 | 現況 |
| :--- | :--- | :--- | :---: |
| **`agentfs` / `redoxfs`** | **原生 FUSE 掛載 + 本機免 kext NFSv3 掛載** | 讓 `git`、`ripgrep`、Python、`ffmpeg` 不需改碼即可直接讀寫單一 `.img`，同時底層仍維持單一 FD | FUSE ✅ 已完成；NFSv3 ⏳ 已規劃 (`docs/nfs_mount_design.md`) |
| **`zbox` / `agentfs` / `redb`** | **CoW Snapshot 與唯讀基底 Overlay / Fork** | 搭配現有 `oifs_mcp`，讓 AI Coding Agent 在毫秒內建立可回滾的單檔工作區沙盒 | ⏳ 待實作 |
| **`redoxfs`** | **Metadata Checksum（Inode / Indirect Block CRC32C）** | 目前 OIFS 的 WAL 有 CRC32C，但主表 Inode 與間接指標塊尚未加檢驗碼；在 256B Inode 保留欄位加入 4B CRC32C 可主動偵測靜默位元翻轉（Bit Rot） | ⏳ 待實作 |
| **`zbox` / `dwarfs`** | **選配跨檔案 64 KiB Content-Addressed 去重** | 結合現有 `BLAKE2b` 與 64 KiB 分塊引擎，對重複資料集、模型檔與多版本備份可再省數倍空間 | ⏳ 待實作 |

---

## 四、優化路線圖與進度（依 ROI 排序）

1. **✅ P0（已完成）— `FileBacking` 精準區間寫回與連續區塊批次寫入**：
   - `JournalRegionMut` 僅 `pwrite` 變更的 Header + Frame 區間（從 132 KiB 降至 ~1 KiB）；`BitmapSliceMut` 縮窄髒位元組寫回範圍。
   - `write_buffer_at_offset` 非 Journaled 路徑新增 `try_write_contiguous_fresh_file`，一次配置連續區塊與間接指標寫回，將 `FileIo Legacy 16M W` 從 **11.96 ms → 1.56 ms（7.7× 加速）**。
2. **✅ P1（已完成）— 單交易 `create_file_with_data` 與 `ReadSession` / `FileReadHandle`**：
   - `DiskManager::create_file_with_data`（及 `create_file_with_data_with_filters`）合併建檔與寫入為單一寫鎖／單筆 WAL 交易；`Strict` 模式下 `<= 8 KiB` 小檔直接 data-journal 進 WAL，使 `FileIo+WAL Strict` 吞吐從 **109.3 → 251.9 files/s（2.31× 加速）**。
   - `DiskManager::begin_read()` → `ReadSession` / `FileReadHandle` 消除高頻隨機讀的重複 `RwLock` 爭用與 Inode 查找，使 `4T8kR` 降至 **0.56 ms (`Mmap`) / 0.72 ms (`FileIo`)**，全面超越 `redb`（0.93 ms）與 `LMDB`（0.97 ms）。
3. **✅ P2（已完成）— `FileIo` 有界 64-Shard User-Space Data Page Cache**：
   - 內建預設 32 MiB（可透過 `OIFS_FILE_IO_CACHE_MB` 或 `DiskManager::set_file_io_cache_bytes` 動態調整或關閉）Second-Chance Clock 分片頁快取，對 `> 64 KiB` 順序掃描自動旁路防污染，並於每次寫入／截斷／刪除／WAL 套用維持嚴格一致性；`FileIo` 熱快取 `5k4KR` 降至 **0.79 ms**、`4T8kR` 降至 **0.72 ms**。
4. **✅ P2.5（已完成）— Modern C++17/20、Inline Fast-Path 非同步、嚴格唯讀模式與純 Rust 跨平台 FUSE**：
   - `include/oifs.hpp`（`oifs::PinnableSlice` 零拷貝、RAII session/handle、C++ 多行程×多執行緒壓測）、`OifsAsyncTask`（Inline Fast-Path 讀 + Group-Commit 寫）、`open_read_only*`（`F_RDLCK` + `PROT_READ` 零寫入掛載）。
   - `src/fuse.rs`（Linux `/dev/fuse` + macOS `macFUSE 5.5.0` `SCM_RIGHTS` 掛載、動態 Linux `FUSE_MAX_PAGES`/`max_pages_limit` 協商、102 Kani Proofs、`tests/fuse_live_mount_test.rs` 實機驗證）。
5. **⏳ P3（下一階段）— 免 kext 本機 Pure-Rust NFSv3 掛載 + POSIX 原子 `rename` + CoW Snapshot / Overlay**：
   - 依 `docs/nfs_mount_design.md` 實作單一 loopback TCP port 的 Pure-Rust NFSv3 Server（免 `rpcbind`、免 kext、免重開機），搭配 POSIX 原子 `rename` 與 CoW Snapshot / Overlay，切入 AI Agent 單檔沙盒與資料集即掛即用場景（完整資料庫級 `WriteTransaction` 因 128 KiB WAL ring 與檔案系統定位，暫不列為優先）。
