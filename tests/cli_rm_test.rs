//! CLI tests for the `rm` subcommand, exercised against both a legacy image and a
//! journaled image (so the delete goes through the WAL-first path).

use std::fs;
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

/// Each test drives several real CLI processes against a session (flock + UDS
/// rendezvous). Running them concurrently within one binary makes those
/// rendezvous handshakes contend nondeterministically, so the tests serialize.
static CLI_LOCK: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    CLI_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn oifs() -> Command {
    Command::new(env!("CARGO_BIN_EXE_oifs"))
}

fn run(args: &[&str]) -> std::process::Output {
    oifs().args(args).output().expect("failed to run oifs")
}

fn seed_file(image: &str, remote_name: &str) {
    // Use a flat, unique host filename: a nested remote name like "a/one.txt"
    // would otherwise try to write "a/one.txt.host" on the host, where "a" may not exist.
    let host = format!("hostseed_{}.tmp", remote_name.replace('/', "_"));
    fs::write(&host, format!("contents of {remote_name}")).expect("write host file");
    let out = run(&["--image", image, "put", &host, remote_name]);
    assert!(
        out.status.success(),
        "put {remote_name} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = fs::remove_file(&host);
}

fn ls_names(image: &str) -> Vec<String> {
    let out = run(&["--image", image, "ls"]);
    assert!(out.status.success(), "ls failed");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let kind = it.next()?;
            let name = it.next()?;
            match kind {
                "d" | "-" => Some(name.to_string()),
                _ => None,
            }
        })
        .collect()
}

fn setup(image: &str, journal: bool) {
    let _ = fs::remove_file(image);
    let _ = fs::remove_file(format!("{image}.master"));
    let mut args = vec!["--image", image, "create", "--size", "10"];
    if journal {
        args.push("--journal");
    }
    let out = run(&args);
    assert!(out.status.success(), "create failed: {out:?}");
}

#[test]
fn test_cli_rm_removes_file_legacy_image() {
    let _guard = serial();
    let image = "test_cli_rm.img";
    setup(image, false);
    seed_file(image, "a.txt");
    seed_file(image, "b.txt");
    assert_eq!(ls_names(image).len(), 2);

    let out = run(&["--image", image, "rm", "a.txt"]);
    assert!(out.status.success(), "rm failed: {out:?}");

    let names = ls_names(image);
    assert_eq!(names, vec!["b.txt".to_string()]);
    let _ = fs::remove_file(image);
}

#[test]
fn test_cli_rm_journaled_image() {
    let _guard = serial();
    let image = "test_cli_rm_journal.img";
    setup(image, true);
    seed_file(image, "a.txt");
    seed_file(image, "b.txt");
    assert_eq!(ls_names(image).len(), 2);

    let out = run(&["--image", image, "rm", "a.txt"]);
    assert!(out.status.success(), "rm failed: {out:?}");
    assert_eq!(ls_names(image), vec!["b.txt".to_string()]);

    // The journaled image must still pass fsck after a CLI delete.
    let out = run(&["--image", image, "fsck"]);
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("CLEAN"),
        "fsck must report CLEAN: {out:?}"
    );
    let _ = fs::remove_file(image);
}

#[test]
fn test_cli_rm_missing_path_fails() {
    let _guard = serial();
    let image = "test_cli_rm_missing.img";
    setup(image, false);
    let out = run(&["--image", image, "rm", "nope.txt"]);
    assert!(!out.status.success(), "rm of a missing path must fail");
    assert!(String::from_utf8_lossy(&out.stderr).contains("not found"));
    let _ = fs::remove_file(image);
}

#[test]
fn test_cli_rm_refuses_root() {
    let _guard = serial();
    let image = "test_cli_rm_root.img";
    setup(image, false);
    let out = run(&["--image", image, "rm", "/"]);
    assert!(!out.status.success(), "rm / must be refused");
    let _ = fs::remove_file(image);
}

#[test]
fn test_cli_rm_empty_directory_ok_and_non_empty_needs_recursive() {
    let _guard = serial();
    let image = "test_cli_rm_dir.img";
    setup(image, true);

    // Empty directory: plain rm succeeds.
    let out = run(&["--image", image, "mkdir", "emptydir"]);
    assert!(out.status.success());
    let out = run(&["--image", image, "rm", "emptydir"]);
    assert!(out.status.success(), "empty dir rm should succeed: {out:?}");

    // Non-empty directory: plain rm must refuse.
    let out = run(&["--image", image, "mkdir", "full"]);
    assert!(out.status.success());
    seed_file(image, "full/child.txt");
    assert_eq!(ls_names(image).iter().filter(|n| *n == "full").count(), 1);

    let out = run(&["--image", image, "rm", "full"]);
    assert!(!out.status.success(), "non-empty dir rm must refuse");
    assert!(String::from_utf8_lossy(&out.stderr).contains("not empty"));

    // -r removes it.
    let out = run(&["--image", image, "rm", "-r", "full"]);
    assert!(out.status.success(), "recursive rm should succeed: {out:?}");
    assert!(!ls_names(image).iter().any(|n| n == "full"));
    let _ = fs::remove_file(image);
}

#[test]
fn test_cli_rm_recursive_nested_directory() {
    let _guard = serial();
    let image = "test_cli_rm_nested.img";
    setup(image, true);

    let out = run(&["--image", image, "mkdir", "a"]);
    assert!(out.status.success());
    seed_file(image, "a/one.txt");
    seed_file(image, "a/two.txt");

    let out = run(&["--image", image, "rm", "-r", "a"]);
    assert!(out.status.success(), "recursive rm failed: {out:?}");
    assert!(ls_names(image).is_empty(), "everything must be gone");

    let out = run(&["--image", image, "fsck"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("CLEAN"));
    let _ = fs::remove_file(image);
}

#[test]
fn test_cli_rm_json_output() {
    let _guard = serial();
    let image = "test_cli_rm_json.img";
    setup(image, false);
    seed_file(image, "j.txt");
    let out = run(&["--image", image, "--json", "rm", "j.txt"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("\"ok\":true"), "unexpected json: {text}");
    assert!(
        text.contains("\"removed\":\"j.txt\""),
        "unexpected json: {text}"
    );
    let _ = fs::remove_file(image);
}

#[test]
fn test_cli_create_journal_flag_reported() {
    let _guard = serial();
    let image = "test_cli_journal_flag.img";
    let _ = fs::remove_file(image);
    let out = run(&["--image", image, "create", "--size", "10", "--journal"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("journaling enabled"));

    // A journaled image must reserve blocks: its data start must be larger than a
    // legacy image of the same size (33 reserved blocks).
    let legacy = "test_cli_legacy_flag.img";
    let _ = fs::remove_file(legacy);
    let out = run(&["--image", legacy, "create", "--size", "10"]);
    assert!(out.status.success());

    let sb_of = |p: &str| -> u64 {
        let bytes = fs::read(p).expect("read image");
        // inode_table_block is at a fixed offset in the bincode SuperBlock:
        // magic(4) block_size(4) block_count(8) inode_bitmap(8) data_bitmap(8)
        u64::from_le_bytes(bytes[32..40].try_into().expect("inode_table_block"))
    };
    assert!(
        sb_of(image) > sb_of(legacy),
        "journaled image must reserve journal blocks before the inode table"
    );
    let _ = fs::remove_file(image);
    let _ = fs::remove_file(legacy);
}

#[test]
fn test_cli_rm_then_recreate_same_name() {
    let _guard = serial();
    let image = "test_cli_rm_recreate.img";
    setup(image, true);
    seed_file(image, "cycle.txt");

    let out = run(&["--image", image, "rm", "cycle.txt"]);
    assert!(out.status.success());

    // Re-creating the same name must work and be readable.
    seed_file(image, "cycle.txt");
    let out = run(&["--image", image, "get", "cycle.txt", "cycle.out"]);
    assert!(out.status.success(), "get failed: {out:?}");
    assert_eq!(
        fs::read_to_string("cycle.out").expect("read out"),
        "contents of cycle.txt"
    );
    let _ = fs::remove_file("cycle.out");
    let _ = fs::remove_file(image);
}

#[test]
fn test_cli_rm_leaves_image_usable() {
    let _guard = serial();
    let image = "test_cli_rm_usable.img";
    setup(image, true);
    for n in ["f0", "f1", "f2", "f3"] {
        seed_file(image, n);
    }
    for n in ["f0", "f2"] {
        let out = run(&["--image", image, "rm", n]);
        assert!(out.status.success());
    }
    let mut names = ls_names(image);
    names.sort();
    assert_eq!(names, vec!["f1".to_string(), "f3".to_string()]);

    // Remaining files must still be readable.
    for n in ["f1", "f3"] {
        let out = run(&["--image", image, "get", n, &format!("{n}.out")]);
        assert!(out.status.success(), "get {n} failed");
        let _ = fs::remove_file(format!("{n}.out"));
    }
    let _ = fs::remove_file(image);
}
