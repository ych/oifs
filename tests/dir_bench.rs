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
            let id = dm
                .create_file(d, &format!("data_node_{:05}.bin", i))
                .unwrap();
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

/// Benchmark proving P4.6:
/// 1. `list_dir` warm cache speedup vs cold disk scan.
/// 2. `resolve_parent` & `resolve_path` zero-allocation throughput.
#[test]
#[ignore]
fn bench_p4_6_dir_cache_and_path_resolution() {
    let path = "bench_p4_6.img";
    let _ = std::fs::remove_file(path);
    let n = 5_000;
    {
        let dm = DiskManager::open(path, 50 * 1024 * 1024).unwrap();
        let root = dm.superblock().root_inode;
        let d = dm.create_directory(root, "bench_dir").unwrap();

        println!("--- Populating directory with {n} files ---");
        for i in 0..n {
            let id = dm
                .create_file(d, &format!("file_entry_{:05}.dat", i))
                .unwrap();
            dm.write_data(id, 0, b"benchmark_data", CompressionMode::Never)
                .unwrap();
        }

        // 1. First list_dir: cold pass (scans disk blocks and promotes to cache)
        let t_cold = Instant::now();
        let entries_cold = dm.list_dir(d).unwrap();
        let dur_cold = t_cold.elapsed();
        assert_eq!(entries_cold.len(), n);
        println!(
            "Cold list_dir ({n} entries): {:?} ({:.2} listings/s)",
            dur_cold,
            1.0 / dur_cold.as_secs_f64()
        );

        // 2. Second list_dir: warm pass (P4.6 cache hit, memory-only)
        let t_warm = Instant::now();
        let iterations = 100;
        for _ in 0..iterations {
            let entries_warm = dm.list_dir(d).unwrap();
            assert_eq!(entries_warm.len(), n);
        }
        let dur_warm_total = t_warm.elapsed();
        let dur_warm_avg = dur_warm_total / iterations;
        let speedup = dur_cold.as_secs_f64() / dur_warm_avg.as_secs_f64();
        println!(
            "Warm list_dir ({n} entries, avg of {iterations} runs): {:?} ({:.2} listings/s)",
            dur_warm_avg,
            1.0 / dur_warm_avg.as_secs_f64()
        );
        println!("==> list_dir P4.6 Cache Speedup: {:.2}x faster", speedup);

        // 3. Path resolution & resolve_parent throughput benchmark (zero heap allocation)
        let sub = dm.create_directory(d, "nested_sub").unwrap();
        let _target = dm.create_file(sub, "target.txt").unwrap();

        let path_str = "bench_dir/nested_sub/target.txt";
        let path_iterations = 200_000;

        let t_res = Instant::now();
        for _ in 0..path_iterations {
            let _ = dm.resolve_path(path_str).unwrap();
        }
        let dur_res = t_res.elapsed();
        let res_ops_per_sec = path_iterations as f64 / dur_res.as_secs_f64();
        println!(
            "resolve_path ({path_iterations} iterations on 3-tier path): {:?} ({:.0} lookups/s, {:.2} ns/op)",
            dur_res,
            res_ops_per_sec,
            dur_res.as_nanos() as f64 / path_iterations as f64
        );

        let t_parent = Instant::now();
        for _ in 0..path_iterations {
            let _ = dm.resolve_parent(path_str).unwrap();
        }
        let dur_parent = t_parent.elapsed();
        let parent_ops_per_sec = path_iterations as f64 / dur_parent.as_secs_f64();
        println!(
            "resolve_parent ({path_iterations} iterations on 3-tier path): {:?} ({:.0} lookups/s, {:.2} ns/op)",
            dur_parent,
            parent_ops_per_sec,
            dur_parent.as_nanos() as f64 / path_iterations as f64
        );
    }
    let _ = std::fs::remove_file(path);
}
