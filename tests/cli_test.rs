use oifs::disk::DiskManager;
use oifs::allocator::BlockAllocator;
use oifs::directory::DirectoryEntry;
use oifs::inode::{FileType, Inode};
use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn test_cli_ls() {
    let path_str = "test_fs_cli.img";
    let path = Path::new(path_str);
    let total_size = 10 * 1024 * 1024; // 10MB

    if path.exists() {
        fs::remove_file(path).unwrap();
    }

    // 1. Setup FS with a file
    /*
    {
        let mut dm = DiskManager::open(path, total_size).expect("Failed to create");
        
        // Update root inode (Inode 0)
        let mut root_inode = dm.read_inode(0).expect("Read root inode");
        root_inode.mode = FileType::Directory;

        // Allocate a block for directory data
        let mut block_alloc = dm.data_block_allocator();
        let dir_block_id = block_alloc.allocate().expect("Allocate block");
        root_inode.blocks[0] = dir_block_id;

        // Allocate a inode for a file "hello.txt"
        let mut inode_alloc = dm.inode_allocator();
        let file_inode_id = inode_alloc.allocate().expect("Allocate inode");
        
        // Write file inode
        let mut file_inode = Inode::new(FileType::File);
        file_inode.size = 123;
        file_inode.modified_at = 1672531200; // Some timestamp
        dm.write_inode(file_inode_id, &file_inode).expect("Write file inode");

        // Write directory entry into dir_block
        if let Some(block_data) = dm.get_block_mut(dir_block_id) {
            let entry = DirectoryEntry {
                inode: file_inode_id,
                hash: 0, // Mock hash
                name: "hello.txt".to_string(),
            };
            let mut cursor = std::io::Cursor::new(block_data);
            entry.serialize_into(&mut cursor).expect("Serialize entry");
        }

        // Save root inode
        dm.write_inode(0, &root_inode).expect("Write root inode");
    }
    */
    
    // Use high level API for setup instead
    {
        let dm = DiskManager::open(path, total_size).expect("Setup");
        let root = dm.resolve_path(".").unwrap();
        // create_file checks if file exists, if it does it might error? No, checking impl.
        match dm.create_file(root, "hello.txt") {
            Ok(id) => {
                 let data = vec![0u8; 123];
                 dm.write_data(id, 0, &data).unwrap();
            },
            Err(_) => {
                // If it exists (e.g. failed cleanup), try to lookup?
                // But we remove file at start.
            }
        }
    }

    // 2. Run CLI
    let output = Command::new("cargo")
        .args(&["run", "--bin", "oifs", "--", "--image", path_str, "ls"])
        .output()
        .expect("Failed to execute command");

    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();

    println!("STDOUT:\n{}", stdout);
    println!("STDERR:\n{}", stderr);

    assert!(output.status.success());
    assert!(stdout.contains("hello.txt"));
    assert!(stdout.contains("123"));

    // Cleanup
    fs::remove_file(path).unwrap();
}
