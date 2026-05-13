# OIFS (O's Inode File System)

OIFS 是一個使用 Rust 編寫的簡單 Inode 檔案系統實作。它支援基本的檔案操作、目錄管理、並發訪問保護 (Thread-safe)，以及透過 FFI 供 C/C++ 呼叫。

## 功能特色 (Features)

*   **Inode-based Architecture**: 採用標準的 Inode 設計管理檔案與目錄。
*   **Encryption Support** 🔒:
    *   XChaCha20-Poly1305 AEAD 加密演算法
    *   Argon2id 密碼金鑰衍生
    *   支援加密與壓縮同時使用
    *   每個檔案使用唯一的 Nonce
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
3. **記憶體安全**: 
   *   加密金鑰使用 `zeroize` crate 在釋放時自動清零
   *   但 Rust 無法保證記憶體不會被交換到磁碟（swap）
   *   建議使用加密的 swap 或停用 swap 以獲得最大安全性
4. **Nonce 唯一性**: 每個檔案使用密碼學安全隨機數產生器（CSPRNG）產生唯一 Nonce
5. **加密與壓縮**: 資料先壓縮後加密，確保壓縮效率不受影響

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
