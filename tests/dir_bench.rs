use oifs::disk::{CompressionMode, DiskManager};
use std::time::Instant;

/// Ad-hoc benchmark (ignored by default): measures create, warm lookup,
/// cold (fresh process cache) lookup and negative lookup in a 10k-entry directory.
#[test]
#[ignore]
fn bench_dir_scaling() {
    let path = "bench_dir_scaling.img";
    let _ = std::fs::remove_file(path);
    let n = 10_000;
    {
        let dm = DiskManager::open(path, 50 * 1024 * 1024).unwrap();
        let root = dm.superblock().root_inode;
        let d = dm.create_directory(root, "d").unwrap();
        let t = Instant::now();
        for i in 0..n {
            let id = dm.create_file(d, &format!("data_node_{:05}.bin", i)).unwrap();
            dm.write_data(id, 0, b"x", CompressionMode::Never).unwrap();
        }
        println!("create {n}: {:?}", t.elapsed());
        let t = Instant::now();
        for i in 0..n {
            dm.lookup(d, &format!("data_node_{:05}.bin", i)).unwrap();
        }
        println!("warm lookup {n}: {:?}", t.elapsed());
    }
    {
        let dm = DiskManager::open(path, 0).unwrap();
        let root = dm.superblock().root_inode;
        let d = dm.lookup(root, "d").unwrap();
        let t = Instant::now();
        for i in 0..n {
            dm.lookup(d, &format!("data_node_{:05}.bin", i)).unwrap();
        }
        println!("cold lookup {n}: {:?}", t.elapsed());
        let t = Instant::now();
        for i in 0..n {
            assert!(dm.lookup(d, &format!("missing_{:05}.bin", i)).is_err());
        }
        println!("negative lookup {n}: {:?}", t.elapsed());
    }
    let _ = std::fs::remove_file(path);
}
