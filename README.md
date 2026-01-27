# OIFS (O's Inode File System)

OIFS 是一個使用 Rust 編寫的簡單 Inode 檔案系統實作。它支援基本的檔案操作、目錄管理、並發訪問保護 (Thread-safe)，以及透過 FFI 供 C/C++ 呼叫。

## 功能特色 (Features)

*   **Inode-based Architecture**: 採用標準的 Inode 設計管理檔案與目錄。
*   **Crash Safety**:
    *   Metadata 操作 (如 `create`, `mkdir`, `delete`) 支援同步寫回 (Sync-on-write)。
    *   使用 `mmap` 的 flush 機制確保資料在崩潰時不遺失。
*   **Concurrency**:
    *   內部使用 `Arc<Mutex<>>` 實現執行緒安全 (Thread-Safe)。
    *   支援多執行緒同時操作 (如 `tests/concurrency_test.rs` 所示)。
*   **CLI Tool**: 提供完整的命令列工具進行映像檔操作。
*   **C API (FFI)**: 提供 C 語言介面庫 (`liboifs.so`)。

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
cargo run --bin oifs -- create --image disk.img --size 10
```

### 2. 匯入檔案 (Import File)
將本機檔案 `hello.txt` 匯入到映像檔中：
```bash
touch hello.txt && echo "Hello World" > hello.txt
cargo run --bin oifs -- put --image disk.img --host-path hello.txt --remote-name hello.txt
```

### 3. 建立目錄 (Make Directory)
在映像檔中建立一個新目錄：
```bash
cargo run --bin oifs -- mkdir --image disk.img --dir-name documents
```

### 4. 列出檔案 (List Files)
列出根目錄下的檔案與資料夾 (支援遞迴 `-r`)：
```bash
cargo run --bin oifs -- ls --image disk.img -r
```

### 5. 匯出檔案 (Export File)
從映像檔中讀取檔案並存回本機：
```bash
cargo run --bin oifs -- get --image disk.img --remote-name hello.txt --host-path downloaded.txt
```

## Rust API 範例

若要在其他 Rust 專案中使用 OIFS：

```rust
use oifs::disk::DiskManager;
use java::path::Path;

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

## 測試 (Testing)

專案包含多種測試套件：
*   **Unit Tests**: 基本功能測試。
*   **Integration Tests**: 整合測試。
*   **Concurrency Test**: 驗證多執行緒寫入與資料完整性。
*   **FFI Test**: 驗證 C 語言介面。

執行所有測試：
```bash
cargo test
```
