# OIFS 安全性審查、待修 Bug 與功能 Backlog（基準版本：v1.5.0）

> 記錄日期：2026-11-25（基於 `v1.5.0` / `8d163dc`）  
> 本文件收錄全程式碼庫靜態審查確認存在之安全問題與實質 Bug（#1–#13），以及使用者自帶 Zstd Dictionary 壓縮功能的設計規劃，供後續排程修復與實作追蹤。

---

## 一、安全性與 Bug 審查待修清單

### 🔴 High（崩潰一致性 / 記憶體安全 / 本機權限安全）

- [x] **#1 WAL 環形緩衝區 wrap-around 未同步尾段清零區（`Mmap + Strict` 崩潰遺失交易或重放舊交易）**
  - **位置**：`src/disk.rs:2728-2746`、`src/journal.rs:1501-1526`、`src/journal.rs:1581-1589`
  - **成因**：當 `JournalRing::append_inner` 觸發 wrap-around 時，會先將 `[old_head..ring_bytes]` 填零並記錄 `last_wrap_pad`，再把新 frame 寫在 offset `0..len`。在預設 `StorageMode::Mmap` + `DurabilityMode::Strict` 下，`commit_journal_tx` 僅呼叫 `flush_range(start, (off - start) + len)`（只刷 header + `0..len`），漏刷尾段 `[ring_base + old_head .. ring_base + ring_bytes]` 的清零區。
  - **影響**：斷電後若尾段仍殘留上一圈的舊資料，`recover` 檢查 `buf.iter().all(|&b| b == 0)` 會失敗，導致提早中止而丟棄剛提交的 Strict 交易，甚至誤將上一圈殘留的合法舊 frame 重新套用（復活已刪檔案或破壞 bitmap）。
  - **修復方向**：保留 `wrap_pad`，在 `Mmap + Strict` 路徑刷 header 之前，先對 `[ring_base + pad .. ring_base + ring_bytes]` 執行 `flush_range`。

- [x] **#2 `recover_journal` 在重放結果尚未 `flush()` 前就清空 WAL header**
  - **位置**：`src/disk.rs:2009-2029`
  - **成因**：Phase 2 逐一 `apply_journal_op` 寫入 mmap / page cache 後，未呼叫 `backing.flush()?`，Phase 3 就立刻將 `head = 0, tail = 0, cleanly_unmounted = true` 寫回 journal header。
  - **影響**：若復原剛完成即斷電，header 可能先落盤顯示乾淨無待重放交易，但 Phase 2 重放的 inode / bitmap / 目錄頁尚未落盤，導致下次掛載不再重放而永久遺失已提交資料。
  - **修復方向**：在 Phase 2 結束、Phase 3 重置 header 之前補上 `backing.flush()?`，寫入乾淨 header 後再刷 header 區塊。

- [x] **#3 C++20 `co_await`（`AsyncFuture::Awaiter`）提早取消協程會繞過 Drop Barrier 並導致 Use-After-Free**
  - **位置**：`include/oifs.hpp:888-947`
  - **成因**：
    - `~Awaiter()` 僅設 `state_->cancelled = true` 即返回，未等待背景操作結束；且 detached `std::thread` 仍握有一份 `st->fut`（`shared_ptr<OIFSAsyncFuture>`），使得 `~AsyncFuture()` 不會觸發 Rust `oifs_async_free` 的等待屏障。若協程 frame 提早銷毀，frame 內的 `read_at_async` 緩衝區先被釋放，但 Rust worker 仍繼續寫入該位址（UAF）。
    - detached thread 在解鎖 `st->mtx`（`L919`）之後才呼叫 `h.resume()`（`L921`），若協程在此空窗被銷毀，會對已銷毀的 handle 呼叫 `resume()`（UAF）。
  - **修復方向**：取消 detached thread 模式，改以 Rust 側 completion callback / waker 機制，或在 `~Awaiter()` 同步等待並保護 `resume()` 生命週期。

- [ ] **#4 多使用者主機安全問題：Local IPC `/tmp/oifs-<uid>` 缺乏擁有者 / symlink 驗證，以及 Loopback NFSv3 無認證存取**
  - **位置**：`src/ipc.rs:210-227`、`src/ipc.rs:772-791`、`src/nfs.rs:932-1014`、`src/nfs.rs:2153-2155`
  - **成因**：
    - 未設 `XDG_RUNTIME_DIR`（如 macOS、多數容器）時，`get_private_runtime_dir()` 使用 `/tmp/oifs-<uid>/oifs`，僅做 `if !dir.exists()` 與 `chmod 0700`，未檢查既有目錄是否為 symlink、擁有者是否為當前 UID、權限是否為 `0700`；且 `.tok.tmp.<pid>` 檔名可預測。多使用者環境下其他使用者可預建該目錄阻斷或劫持本地 IPC。
    - `OifsNfsServer` 監聽 `127.0.0.1:<port>` 且 `dispatch_rpc` 完全不驗證呼叫者身分（接受 `AUTH_NONE` 與任意 `AUTH_SYS`）。在多使用者主機上，任何本機使用者皆可直接連線該 port 讀寫或刪除已解鎖掛載中的加密 OIFS 映像檔。
  - **修復方向**：`get_private_runtime_dir()` 以 `symlink_metadata` 逐層確認 `!is_symlink() && is_dir() && uid == geteuid() && (mode & 0o077) == 0`，`.tok.tmp` 改用隨機名稱並加 `O_NOFOLLOW`。NFSv3 至少需在 CLI / 文件中明確警示多使用者主機風險（或引導改用不開 TCP port 的 FUSE）。

---

### 🟠 Medium（資料遺失 / 並發競態 / 結構損壞）

- [x] **#5 `transfer_file_to` 覆寫時先刪除目標再複製（失敗即丟失原檔），且 Stage 1→2 對來源檔無鎖存在跨檔資料錯亂風險**
  - **位置**：`src/disk/transfer.rs:1319-1414`、`src/disk/transfer.rs:3253-3297`
  - **成因**：
    - 快路徑（`L1412`）與 `transfer_logical_fallback`（`L3267`）在 `overwrite == true` 時都先 `dst.delete_file(...)`，之後才分配與寫入新檔。一旦發生 `NoSpace` 或 I/O 失敗，原有目標檔已被永久刪除。
    - Stage 1 收集 `src` 的物理 block 編號後即釋放 `src_guard`，Stage 2 直接用 `src_fd` 讀原始 offset；若同時有其他執行緒修改 / 截斷 / 重組 /刪除來源檔，這些 block 可能被重新分配給別的檔案，導致寫入目標檔的資料錯亂甚至洩漏他檔內容。
  - **修復方向**：覆寫改以暫存檔 / 未連結 inode 完成後原子替換；並以 `inode_write_gen`（或在 Stage 2 前後比對世代號）偵測來源並發變更並重試或中止。

- [x] **#6 `splice_range_to` 與 `trim_source_collapse_range` 存在跨鎖 TOCTOU，可能以過期 layout 毀損檔案**
  - **位置**：`src/disk/transfer.rs:2159-2345`、`src/disk/transfer.rs:2770-2835`
  - **成因**：
    - `splice_range_to` 在 `L1980` 檢查 `can_splice_fast_path` 後釋放鎖；進入 `L2164` 寫鎖時未再確認 `dst_inode_id` 是否仍已分配、仍為 regular file、或壓縮 / 加密 / filter / seekable 狀態未變更。
    - `trim_source_collapse_range` 在 Stage 1 讀鎖算出 `total_units`、`is_seekable`、`new_size` 後放鎖，Stage 2 寫鎖下重新讀取 `inode` 卻未檢驗是否有並發寫入，直接套用舊 `new_size` 與舊單位數搬移指標。
  - **修復方向**：在 Stage 2 寫鎖下驗證 `is_inode_allocated` 與 `inode_gen` / layout 不變量，若已變更則重試或回退邏輯路徑。

- [ ] **#7 FUSE 與 NFSv3：`fast_parents` 快取競態，以及 `rename` / `unlink` / `rmdir` / `create` TOCTOU**
  - **位置**：`src/fuse.rs:2886-2905`、`src/fuse.rs:3034-3127`、`src/nfs.rs:773-795`、`src/nfs.rs:1438-1476`、`src/nfs.rs:1559-1708`
  - **成因**：
    - `record_parent` 與 `forget_parent` 在 `parent_map.write()` 鎖外更新 `fast_parents`，並發 `lookup` / `rename` / `unlink` 會造成兩邊狀態錯亂，且 `forget_parent` 的 load-check-store 可能誤清 hash 碰撞的另一個 inode。
    - FUSE `rename` 在 `dm.rename` 前先查 `lookup` 判斷 `FUSE_RENAME_NOREPLACE` / `RENAME_EXCL`，並發 `create` 會讓 `NOREPLACE` 靜默覆寫新檔；FUSE `unlink` / `rmdir` 與 NFS `nfs_remove` / `nfs_rmdir` 均在 `delete_file` 寫鎖外檢查型別與空目錄，並發替換會導致誤刪。
    - NFS `nfs_create`（`src/nfs.rs:1438-1454`）在 `CREATE_UNCHECKED` 且未設 `set_size` 時，若目標已是目錄，會錯誤回傳 `NFS3_OK` 與目錄 handle，而非 `NFS3ERR_EXIST` / `NFS3ERR_ISDIR`。
  - **修復方向**：將 `fast_parents` 讀寫包入 `parent_map.write()` 內；在 `DiskManager` 層提供單鎖原子的 `rename_noreplace`、`unlink_file`、`rmdir_empty` 與 `create_or_open_regular_file`。

- [x] **#8 非 Journal 模式 Seekable 64K：`ENOSPC` 殘留懸空指標，以及 `truncate_chunked_64k` 漏刷 bitmap / tail chunk**
  - **位置**：`src/disk/chunked.rs:1720-1817`、`src/disk/chunked.rs:2358-2365`
  - **成因**：
    - 非 journal `commit_prepared_chunked_write` 在迴圈中邊寫 indirect block 邊釋放舊 chunk；若中途因 `NoSpace` 失敗，已釋放的舊 block 仍被磁碟上的 `inode` direct 指標引用，且快取未失效。
    - 非 journal `truncate_chunked_64k` 在 `Strict` / `RangeAsync` 模式下僅同步了 `inode_byte_range`，漏掉 `data_bitmap_byte_range`、indirect blocks 與重新編碼的 tail chunk。
  - **修復方向**：先完成所有新區塊分配與寫入再更新指標並釋放舊區塊（失敗時回滾新區塊）；`truncate_chunked_64k` 補齊所有異動區段的 `sync_mutation_ranges`。

- [ ] **#9 `defragment_safe` 未先 flush / 鎖定導致並發寫入遺失，且同目錄 `output_path` 未阻擋 symlink / 既有檔覆寫**
  - **位置**：`src/disk/maintenance.rs:471-548`、`src/disk/maintenance.rs:642-699`、`src/ipc.rs:1437-1444`
  - **成因**：
    - `defragment_safe` 在 `std::fs::copy(source_path, &temp_path)` 前未呼叫 `self.flush()`，且全程未阻擋並發寫入，重組期間的寫入在 `reload_from_disk` 後會被靜默覆蓋。
    - `output_path`（可由 `IpcRequest::Defragment` 傳入）僅驗證父目錄相同且不等於 `backing`，未檢查目標是否為既有 symlink（`std::fs::copy` 會跟隨 symlink 覆寫目錄外檔案）或同目錄內其他重要檔案。
  - **修復方向**：複製前先 `flush()` 並序列化寫入；以 `create_new(true)` + `O_NOFOLLOW` 建立暫存檔，並在 IPC 端忽略 / 禁止遠端指定任意 `output_path`。

---

### 🟡 Low（防禦性強化與邊界瑕疵）

- [ ] **#10 `set_logical_block_ptr_in_place` 缺少 `is_data_block()` 檢查**
  - **位置**：`src/disk/transfer.rs:3408-3448`
  - **說明**：遇到損壞 / 惡意映像的非法 indirect pointer 時可能覆寫 superblock、bitmap、WAL 或 inode table。

- [ ] **#11 `StorageMode::FileIo` 的 `write_block_ptr` 靜默吞掉 `pwrite` I/O 錯誤**
  - **位置**：`src/storage.rs:1513-1546`
  - **說明**：回傳 `()` 而非 `io::Result<()>`，底層寫入失敗不會向上傳播。

- [ ] **#12 `for_each_inode_block` 未過濾越界 block pointer**
  - **位置**：`src/disk.rs:2798-2822`、`src/disk.rs:7826-7828`
  - **說明**：在損壞映像上刪除 / 覆寫檔案可能將越界 `SetDataBitmap` 寫進 WAL，造成後續掛載復原持續失敗。

- [ ] **#13 其他小型防禦性瑕疵**
  - `src/disk/chunked.rs:79-257`：`set_logical_chunk_entry` 殘留未使用的 `ops: Option<&mut Vec<MetadataOp>>` 參數（若傳入 `Some` 會違反 WAL 先寫原則，建議移除）；`L1623` / `L1763` 的 `new_start_blk as u32` 應改用 `u32::try_from`。
  - `src/bin/oifs.rs:861-881`：`oifs put` 先 `create_file` 再讀 host 檔案與驗證 `--filter`，失敗會殘留 0-byte 空檔。
  - `src/nfs.rs:1100-1106`：`SETATTR` 解析 `sattrguard3` 後忽略 ctime 比對（未回 `NFS3ERR_NOT_SYNC`）。
  - `src/ipc.rs:348-357`、`src/disk.rs:1936-1940`：手寫比對迴圈可統一改用帶 `black_box` 的 `crate::encryption::ct_eq_slice`。
  - `src/ffi.rs:1463-1497`：`OIFSReadSession` / `OIFSFileHandle` 用 `Arc` 持有 `!Send` 的 `RwLockReadGuard`，若 C/C++ 將 `PinnableSlice` 跨執行緒釋放會在非加鎖執行緒解鎖 `RwLock`（POSIX UB，應於文件標明同執行緒釋放限制或改用不綁定執行緒的鎖）。

---

## 二、功能 Backlog：支援使用者自帶 Zstd Dictionary 壓縮

### 背景與可行性

- 專案已依賴 `zstd = "0.14"`（`zstd-safe 8.4.0` + Zstd `1.6.0`），原生提供 `EncoderDictionary`（`CDict`）、`DecoderDictionary`（`DDict`）、`get_dict_id_from_dict`、`get_dict_id_from_frame` 與 `zstd::dict::from_samples`，**零新增外部 crate 依賴**。
- 標準 Zstd 訓練字典自帶 32-bit `dict_id`，壓縮時會自動寫入 Zstd frame header，解壓端可用 `get_dict_id_from_frame` 自動匹配對應 `DDict`，不須更動 SuperBlock 或 Inode 格式。
- 若需支援 raw-content 字典（Zstd `dict_id == 0`）或於 `stat` 直接呈現字典 ID，`Inode` v2 尾端（`src/inode_format.rs:39-41`，`[171..256]`）尚有 85 bytes 零填充保留空間，`flags: u32` 目前亦僅使用 `0x0001`（`INODE_FLAG_SEEKABLE_64K`），可向後相容擴充。

### 設計選項

1. **方案 A：外部自帶字典（Runtime 註冊）**
   - API 範例：`dm.register_zstd_dict(&dict_bytes)` / 設定預設壓縮字典 / CLI `--dict <path>` / FFI & Modern C++ 綁定。
   - 優點：零 on-disk 格式變動、Read-Only 映像直接可用、天然支援多組 `dict_id` 共存。
   - 注意：讀取端必須註冊同一份字典，否則回傳結構化錯誤（如 `DiskManagerError::MissingDictionary { dict_id }`）。
2. **方案 B：可選將字典存進 `.img` 內部（自包含，並相容外部覆寫）**
   - 將字典存於映像檔內部保留區或系統目錄（加密映像檔會一併加密），開啟 `.img` 時自動載入，維持單檔可攜性。
   - 需同步考量 `verify_integrity`、`defragment` 與刪除/更新字典的引用安全。
3. **可選輔助能力**：提供 `train_zstd_dict` API / `oifs dict train` CLI，直接以 `.img` 內現有樣本檔訓練字典。

### 實作關鍵細節 Checklist

- [ ] **小檔壓縮門檻調整**：啟用字典時，於 `Auto` 模式降低或取消現行 `8192` bytes 下限（保留 anti-inflation `compressed.len() < data.len()` 檢查），讓幾百 bytes ～ 幾 KiB 的小檔發揮 Zstd 字典最大壓縮率。
- [ ] **`src/zstd_pool.rs` 整合**：以 `Arc<CDict>`（keyed by `(dict_id, level)`）與 `Arc<DDict>`（keyed by `dict_id`）跨執行緒零鎖共享，搭配現有 thread-local `CCtx`/`DCtx`（`compress_using_cdict` / `decompress_using_ddict`），維持 `par_for_index` 多執行緒效能。
- [ ] **全檔與 Seekable 64K 雙路徑支援**：涵蓋 `src/disk.rs` 全檔壓縮與 `src/disk/chunked.rs` 64 KiB chunk 壓縮／解壓。
- [ ] **跨映像快速搬移相容性（`src/disk/transfer.rs`）**：在 `transfer_file_to` / `splice_range_to` 的 fast-path 判斷中，若來源檔案使用字典而目標映像未持有相同 `dict_id`，自動回退 `LogicalReencode`，避免複製出無法解壓的區塊。
- [ ] **安全與資源上限**：限制單一字典最大容量（例如 16 MiB）與快取 `CDict`/`DDict` 數量上限，防止惡意或過大字典導致 OOM；解壓找不到對應 `dict_id` 時回傳明確錯誤。
