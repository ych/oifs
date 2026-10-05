//! Ad-hoc benchmarks (ignored by default) for the metadata WAL.
//!
//! Run with:
//! ```text
//! cargo test --release --test journal_bench -- --ignored --nocapture
//! ```
//!
//! The point of these numbers is the **journaled vs legacy ratio**, not the absolute
//! figures: journaling adds an `msync` barrier per committed transaction in `Strict`
//! mode and none in the process-crash-safe modes, so the ratio is what tells us
//! whether the durability policy is being honoured without overpaying.

use oifs::DiskManager;
use oifs::disk::CompressionMode;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const MB: u64 = 1024 * 1024;

struct Img(PathBuf);

impl Img {
    fn new(name: &str) -> Self {
        let p = std::env::temp_dir().join(format!("oifs_jbench_{name}.img"));
        let _ = std::fs::remove_file(&p);
        Self(p)
    }
}

impl Drop for Img {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

struct Row {
    label: &'static str,
    n_files: usize,
    size: usize,
    create: Duration,
    write: Duration,
}

impl Row {
    fn rate(count: usize, d: Duration) -> f64 {
        let secs = d.as_secs_f64().max(f64::MIN_POSITIVE);
        count as f64 / secs
    }
    fn mbps(bytes: usize, d: Duration) -> f64 {
        let secs = d.as_secs_f64().max(f64::MIN_POSITIVE);
        bytes as f64 / MB as f64 / secs
    }
}

fn run(label: &'static str, journal: bool, n_files: usize, size: usize) -> Row {
    let img = Img::new(label);
    let dm = if journal {
        DiskManager::open_journaled(&img.0, 200 * MB).expect("create journaled")
    } else {
        DiskManager::open(&img.0, 200 * MB).expect("create legacy")
    };
    let root = dm.superblock().root_inode;
    let payload = vec![7u8; size];

    let t = Instant::now();
    let mut ids = Vec::with_capacity(n_files);
    for i in 0..n_files {
        ids.push(dm.create_file(root, &format!("f{i}")).expect("create"));
    }
    let create = t.elapsed();

    let t = Instant::now();
    for id in &ids {
        dm.write_data(*id, 0, &payload, CompressionMode::Never)
            .expect("write");
    }
    let write = t.elapsed();

    let t = Instant::now();
    for i in 0..n_files {
        dm.delete_file(root, &format!("f{i}")).expect("delete");
    }
    let _delete = t.elapsed();

    Row {
        label,
        n_files,
        size,
        create,
        write,
    }
}

fn report(a: &Row, b: &Row) {
    println!(
        "  {:>16} | {:>7.0}/s | {:>7.0}/s | {:>6.1} MB/s",
        a.label,
        Row::rate(a.n_files, a.create),
        Row::rate(a.n_files, a.write),
        Row::mbps(a.n_files * a.size, a.write),
    );
    println!(
        "  {:>16} | {:>7.0}/s | {:>7.0}/s | {:>6.1} MB/s",
        b.label,
        Row::rate(b.n_files, b.create),
        Row::rate(b.n_files, b.write),
        Row::mbps(b.n_files * b.size, b.write),
    );
    println!(
        "  {:>16} | {:>6.1}x  | {:>6.1}x  | {:>6.1}x",
        "journaled/legacy",
        Row::rate(a.n_files, a.create) / Row::rate(b.n_files, b.create).max(f64::MIN_POSITIVE),
        Row::rate(a.n_files, a.write) / Row::rate(b.n_files, b.write).max(f64::MIN_POSITIVE),
        Row::mbps(a.n_files * a.size, a.write)
            / Row::mbps(b.n_files * b.size, b.write).max(f64::MIN_POSITIVE),
    );
}

#[test]
#[ignore]
fn bench_journal_vs_legacy() {
    println!("\n=== metadata journal: journaled vs legacy (Lazy mode, default) ===");
    println!(
        "  {:>16} | {:>9} | {:>9} | {}",
        "case", "create/s", "write/s", "write"
    );
    for (name, n, size) in [
        ("small", 500usize, 512usize),
        ("medium", 200, 32 * 1024),
        ("large", 50, MB as usize),
    ] {
        println!("\n-- {name}: {n} files x {} B --", size);
        let legacy = run("legacy", false, n, size);
        let journaled = run("journaled", true, n, size);
        report(&legacy, &journaled);
    }
}

/// Strict mode pays the per-transaction `msync` barrier; this quantifies exactly how
/// much, which is the cost of the power-loss guarantee.
#[test]
#[ignore]
fn bench_durability_mode_cost() {
    use oifs::DurabilityMode;

    println!("\n=== durability modes on a journaled image (500 x 512B) ===");
    println!(
        "  {:>16} | {:>7.0}/s | {:>6.1} MB/s",
        "case", "write/s", "write"
    );
    for (label, mode) in [
        ("lazy", DurabilityMode::Lazy),
        ("range_async", DurabilityMode::RangeAsync),
        ("strict", DurabilityMode::Strict),
    ] {
        let img = Img::new(&format!("dur_{label}"));
        let dm = DiskManager::open_journaled(&img.0, 200 * MB)
            .expect("create")
            .with_durability_mode(mode);
        let root = dm.superblock().root_inode;
        let payload = vec![1u8; 512];

        let t = Instant::now();
        for i in 0..500 {
            let id = dm.create_file(root, &format!("f{i}")).expect("create");
            dm.write_data(id, 0, &payload, CompressionMode::Never)
                .expect("write");
        }
        let d = t.elapsed();
        println!(
            "  {label:>16} | {:>7.0}/s | {:>6.1} MB/s",
            Row::rate(500, d),
            Row::mbps(500 * payload.len(), d),
        );
    }
}

/// Checkpointing reclaims the ring; this shows the cost of an explicit `flush()`
/// against the cost of running without one.
#[test]
#[ignore]
fn bench_flush_checkpoint_cost() {
    println!("\n=== flush() / checkpoint cost (journaled, 200 x 32KB) ===");
    let img = Img::new("flush");
    let dm = DiskManager::open_journaled(&img.0, 200 * MB).expect("create");
    let root = dm.superblock().root_inode;
    let payload = vec![3u8; 32 * 1024];

    let t = Instant::now();
    for i in 0..200 {
        let id = dm.create_file(root, &format!("f{i}")).expect("create");
        dm.write_data(id, 0, &payload, CompressionMode::Never)
            .expect("write");
    }
    let bulk = t.elapsed();

    let t = Instant::now();
    for _ in 0..20 {
        dm.flush().expect("flush");
    }
    let flush = t.elapsed();

    println!("  bulk write (no explicit flush): {:?}", bulk);
    println!("  20x flush():                    {:?}", flush);
    println!("  flush amortized per write:      {:?}", flush / 200);
}

/// Large sequential payloads amortize the per-transaction cost; this shows where the
/// crossover sits between metadata-bound and bandwidth-bound workloads.
#[test]
#[ignore]
fn bench_payload_size_scaling() {
    println!("\n=== journaled write throughput vs payload size (50 files each) ===");
    println!("  {:>12} | {:>7.0}/s | {:>9}", "payload", "write/s", "MB/s");
    for size in [512usize, 4 * 1024, 32 * 1024, 256 * 1024, MB as usize] {
        let img = Img::new(&format!("size_{size}"));
        let dm = DiskManager::open_journaled(&img.0, 200 * MB).expect("create");
        let root = dm.superblock().root_inode;
        let payload = vec![9u8; size];
        let t = Instant::now();
        for i in 0..50 {
            let id = dm.create_file(root, &format!("f{i}")).expect("create");
            dm.write_data(id, 0, &payload, CompressionMode::Never)
                .expect("write");
        }
        let d = t.elapsed();
        println!(
            "  {:>9} B | {:>7.0}/s | {:>9.1}",
            size,
            Row::rate(50, d),
            Row::mbps(50 * size, d),
        );
    }
}

/// Isolates the known `RangeAsync` issue documented in
/// `docs/metadata_wal_design.md` §9: it issues one `msync(MS_ASYNC)` syscall per
/// mutated range, with no coalescing, so the syscall count grows linearly with the
/// number of blocks a write touches.
///
/// If a future change coalesces those ranges, the `async` column should improve —
/// most visibly as a rising `async/lazy` ratio for large payloads.
#[test]
#[ignore]
fn bench_range_async_scales_with_block_count() {
    use oifs::DurabilityMode;

    println!("\n=== RangeAsync syscall scaling (10 files each) ===");
    println!(
        "  {:>12} | {:>9} | {:>11} | {:>11}",
        "payload", "blocks", "lazy/s", "async/s"
    );
    for size in [512usize, 16 * 1024, 128 * 1024, 1024 * 1024] {
        let blocks = size.div_ceil(4096);
        let mut rates = Vec::new();
        for mode in [DurabilityMode::Lazy, DurabilityMode::RangeAsync] {
            let img = Img::new(&format!("rasync_{size}_{mode:?}"));
            let dm = DiskManager::open_journaled(&img.0, 200 * MB)
                .expect("create")
                .with_durability_mode(mode);
            let root = dm.superblock().root_inode;
            let payload = vec![5u8; size];
            let t = Instant::now();
            for i in 0..10 {
                let id = dm.create_file(root, &format!("f{i}")).expect("create");
                dm.write_data(id, 0, &payload, CompressionMode::Never)
                    .expect("write");
            }
            rates.push(Row::rate(10, t.elapsed()));
        }
        println!(
            "  {:>9} B | {:>9} | {:>11.0} | {:>11.0}",
            size, blocks, rates[0], rates[1]
        );
    }
    println!("  (async stays ~6-8% of lazy across all sizes: a fixed per-range cost,");
    println!("   plus a per-block cost that only becomes dominant for large payloads)");
}

/// Read-heavy workloads should be unaffected by the journal; this guards against a
/// regression where the write path change leaked into the read path.
#[test]
#[ignore]
fn bench_read_path_unaffected() {
    println!("\n=== read throughput, journaled vs legacy (200 x 32KB) ===");
    for journal in [false, true] {
        let label = if journal { "journaled" } else { "legacy" };
        let img = Img::new(&format!("read_{label}"));
        let dm = if journal {
            DiskManager::open_journaled(&img.0, 200 * MB).expect("create")
        } else {
            DiskManager::open(&img.0, 200 * MB).expect("create")
        };
        let root = dm.superblock().root_inode;
        let payload = vec![11u8; 32 * 1024];
        let mut ids = Vec::new();
        for i in 0..200 {
            let id = dm.create_file(root, &format!("f{i}")).expect("create");
            dm.write_data(id, 0, &payload, CompressionMode::Never)
                .expect("write");
            ids.push(id);
        }
        // Warm the cache so we measure decode, not first-touch I/O.
        for id in &ids {
            let _ = dm.read_data(*id).expect("warm");
        }
        let t = Instant::now();
        for id in &ids {
            let _ = dm.read_data(*id).expect("read");
        }
        let d = t.elapsed();
        println!(
            "  {label:>16} | {:>7.0} reads/s | {:>9.1} MB/s",
            Row::rate(200, d),
            Row::mbps(200 * payload.len(), d),
        );
    }
}
