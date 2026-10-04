use std::fs;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

use oifs::disk::CompressionMode;
use oifs::ipc::{SessionEvent, get_master_info_path, get_socket_path};
use oifs::session::OifsSession;

#[test]
fn test_single_process_direct_mode() {
    let img_path = "test_single_session.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let session = OifsSession::open(img_path, 10 * 1024 * 1024).expect("Open failed");
    assert!(
        session.is_direct(),
        "First session must be in Direct mode (Master)"
    );
    assert_eq!(session.peer_count(), 0);

    let root_id = session.resolve_path(".").expect("Resolve root failed");
    let file_id = session
        .create_file(root_id, "hello.txt")
        .expect("Create file failed");

    let test_data = b"Hello from single process direct mode!";
    session
        .write_data(file_id, 0, test_data, CompressionMode::Auto)
        .expect("Write failed");

    let read_back = session.read_data(file_id).expect("Read failed");
    assert_eq!(read_back, test_data);

    let report = session.verify_integrity().expect("Fsck failed");
    assert!(report.is_clean);

    drop(session);
    let _ = fs::remove_file(img_path);
}

#[test]
fn test_multi_process_transparent_proxy_and_concurrency() {
    let img_path = "test_multi_session.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    // 1. Master process starts
    let master = OifsSession::open(img_path, 10 * 1024 * 1024).expect("Master open failed");
    assert!(master.is_direct(), "Master must be Direct mode");

    // 2. Client 1 connects
    let client1 = OifsSession::open(img_path, 0).expect("Client 1 open failed");
    assert!(!client1.is_direct(), "Client 1 must be Remote mode");

    // 3. Client 2 connects
    let client2 = OifsSession::open(img_path, 0).expect("Client 2 open failed");
    assert!(!client2.is_direct(), "Client 2 must be Remote mode");

    // Wait a brief moment for connection counters
    thread::sleep(Duration::from_millis(50));
    assert_eq!(
        master.peer_count(),
        2,
        "Master should see 2 active peer clients"
    );

    // 4. Concurrent write operations from Client 1 and Client 2
    let c1_handle = {
        let c1 = client1.clone();
        thread::spawn(move || {
            let root = c1.resolve_path(".").unwrap();
            let fid = c1.create_file(root, "c1_file.txt").unwrap();
            let data = b"Data written by client 1";
            c1.write_data(fid, 0, data, CompressionMode::Auto).unwrap();
            (fid, data.to_vec())
        })
    };

    let c2_handle = {
        let c2 = client2.clone();
        thread::spawn(move || {
            let root = c2.resolve_path(".").unwrap();
            let fid = c2.create_file(root, "c2_file.txt").unwrap();
            let data = b"Data written by client 2 with larger content padding 1234567890";
            c2.write_data(fid, 0, data, CompressionMode::Auto).unwrap();
            (fid, data.to_vec())
        })
    };

    let (c1_fid, c1_expected) = c1_handle.join().unwrap();
    let (c2_fid, c2_expected) = c2_handle.join().unwrap();

    // 5. Master verifies and reads data written by clients
    let master_read_c1 = master.read_data(c1_fid).expect("Master read c1 failed");
    assert_eq!(master_read_c1, c1_expected);

    let master_read_c2 = master.read_data(c2_fid).expect("Master read c2 failed");
    assert_eq!(master_read_c2, c2_expected);

    // 6. Cross-client reading: Client 1 reads file created by Client 2
    let c2_lookup = client1
        .lookup(0, "c2_file.txt")
        .expect("Lookup from client1 failed");
    assert_eq!(c2_lookup, c2_fid);
    let c1_reads_c2 = client1
        .read_data(c2_lookup)
        .expect("Read from client1 failed");
    assert_eq!(c1_reads_c2, c2_expected);

    // 7. Directory listing via IPC
    let root_entries = client2.list_dir(0).expect("List dir from client2 failed");
    let names: Vec<String> = root_entries.into_iter().map(|e| e.name).collect();
    assert!(names.contains(&"c1_file.txt".to_string()));
    assert!(names.contains(&"c2_file.txt".to_string()));

    // 8. Dropping client 1 reduces active peer count
    drop(client1);
    thread::sleep(Duration::from_millis(50));
    assert_eq!(
        master.peer_count(),
        1,
        "Master should now see 1 active peer client"
    );

    drop(client2);
    thread::sleep(Duration::from_millis(50));
    assert_eq!(
        master.peer_count(),
        0,
        "Master should now see 0 active peer clients"
    );

    drop(master);
    let _ = fs::remove_file(img_path);
}

#[test]
fn test_event_notification_on_master() {
    let img_path = "test_events.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let master = OifsSession::open(img_path, 10 * 1024 * 1024).expect("Master open failed");
    let event_rx = master
        .take_event_receiver()
        .expect("Must have event receiver");

    // Spawn a peer client in a thread
    let img_path_clone = img_path.to_string();
    let peer_thread = thread::spawn(move || {
        let client = OifsSession::open(&img_path_clone, 0).expect("Client open failed");
        let root = client.resolve_path(".").unwrap();
        let fid = client.create_file(root, "event_test.txt").unwrap();
        client
            .write_data(fid, 0, b"event notification", CompressionMode::Never)
            .unwrap();
        thread::sleep(Duration::from_millis(50));
        drop(client);
    });

    peer_thread.join().unwrap();

    // Verify events arrived in sequence
    let mut events = Vec::new();
    while let Ok(event) = event_rx.recv_timeout(Duration::from_millis(300)) {
        events.push(event);
    }

    let has_connect = events
        .iter()
        .any(|e| matches!(e, SessionEvent::PeerConnected { .. }));
    let has_disconnect = events
        .iter()
        .any(|e| matches!(e, SessionEvent::PeerDisconnected { .. }));
    let has_request = events
        .iter()
        .any(|e| matches!(e, SessionEvent::RequestHandled { .. }));

    assert!(has_connect, "Master should receive PeerConnected event");
    assert!(has_request, "Master should receive RequestHandled events");
    assert!(
        has_disconnect,
        "Master should receive PeerDisconnected event"
    );

    drop(master);
    let _ = fs::remove_file(img_path);
}

#[test]
fn test_stale_socket_recovery() {
    let img_path = "test_stale_recovery.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    // First create image
    {
        let session = OifsSession::open(img_path, 10 * 1024 * 1024).expect("Open failed");
        drop(session);
    }

    // Now artificially create a dead socket file (without a listener)
    let sock_path = get_socket_path(img_path);
    fs::write(&sock_path, b"stale socket placeholder").expect("Write placeholder failed");
    assert!(sock_path.exists(), "Dummy stale socket must exist");

    // Opening session now must detect that socket is dead, remove it, and become Master!
    let new_session = OifsSession::open(img_path, 0).expect("Open with stale socket must recover");
    assert!(
        new_session.is_direct(),
        "Recovered session must be Master (Direct mode)"
    );

    drop(new_session);
    let _ = fs::remove_file(img_path);
}

#[test]
fn test_encrypted_filesystem_with_session() {
    let img_path = "test_enc_session.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let password = "SecretMasterPassword123!";

    // Create encrypted session as Master
    let master = OifsSession::create_encrypted(img_path, 10 * 1024 * 1024, password)
        .expect("Create encrypted failed");
    assert!(master.is_direct());

    // Connect Client session
    let client = OifsSession::open_with_password(img_path, 0, Some(password))
        .expect("Client open encrypted failed");
    assert!(!client.is_direct());

    // Client writes encrypted file
    let root = client.resolve_path(".").unwrap();
    let fid = client.create_file(root, "top_secret.txt").unwrap();
    let secret_payload = b"Top secret data accessed across IPC session";
    client
        .write_data(fid, 0, secret_payload, CompressionMode::Auto)
        .unwrap();

    // Master reads file
    let read_back = master.read_data(fid).unwrap();
    assert_eq!(read_back, secret_payload);

    drop(client);
    drop(master);
    let _ = fs::remove_file(img_path);
}

#[test]
fn test_network_mode_transparent_proxy_and_concurrency() {
    let img_path = "test_network_mode.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    // 1. Master opens in Network mode (binds ephemeral TCP port)
    let master = OifsSession::open_network(img_path, 10 * 1024 * 1024, None)
        .expect("Network master open failed");
    assert!(master.is_direct(), "Network Master must be Direct mode");

    // Verify rendezvous master info file exists
    let master_info_file = get_master_info_path(img_path);
    assert!(master_info_file.exists(), "Rendezvous file must exist");
    let content = fs::read_to_string(&master_info_file).unwrap();
    assert!(
        content.contains("\"addr\""),
        "Rendezvous file must contain addr"
    );

    // 2. Client connects via Network mode
    let client = OifsSession::open_network(img_path, 0, None).expect("Network client open failed");
    assert!(!client.is_direct(), "Network Client must be Remote mode");

    // 3. Client writes data over TCP
    let root = client.resolve_path(".").unwrap();
    let fid = client.create_file(root, "net_file.txt").unwrap();
    let payload = b"Data transmitted seamlessly over TCP network transport";
    client
        .write_data(fid, 0, payload, CompressionMode::Auto)
        .unwrap();

    // 4. Master verifies data
    let read_back = master.read_data(fid).unwrap();
    assert_eq!(read_back, payload);

    // 5. Client reads back data
    let client_read = client.read_data(fid).unwrap();
    assert_eq!(client_read, payload);

    drop(client);
    drop(master);

    // After drop, rendezvous file must be cleaned up
    assert!(
        !master_info_file.exists(),
        "Rendezvous file must be cleaned up on Master drop"
    );
    let _ = fs::remove_file(img_path);
}

#[test]
fn test_network_mode_stale_rendezvous_recovery_and_active_probe() {
    let img_path = "test_net_stale.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    // 1. Create image first
    {
        let session = OifsSession::open_network(img_path, 10 * 1024 * 1024, None).unwrap();
        drop(session);
    }

    // 2. Artificially create a dead master rendezvous file pointing to a dead port
    let master_info_file = get_master_info_path(img_path);
    let dead_info = oifs::ipc::MasterInfo {
        addr: "127.0.0.1:59999".to_string(), // Dead port
        pid: 99999,
    };
    fs::write(
        &master_info_file,
        serde_json::to_string(&dead_info).unwrap(),
    )
    .unwrap();
    assert!(master_info_file.exists());

    // 3. Open in Network mode -> Active Ping Probe discovers port is dead, cleans up file, and promotes to Master!
    let session = OifsSession::open_network(img_path, 0, None)
        .expect("Open with stale rendezvous file should auto-recover");
    assert!(session.is_direct(), "Recovered session must be Master");

    drop(session);
    let _ = fs::remove_file(img_path);
}

#[test]
fn test_cli_network_flag() {
    let img_path = "test_cli_network.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let bin = env!("CARGO_BIN_EXE_oifs");

    // Create via CLI --network
    let res = Command::new(bin)
        .args(["--network", "--image", img_path, "create", "--size", "10"])
        .status()
        .expect("Failed to run create");
    assert!(res.success());

    // Write a test payload file on host
    let host_file = "test_net_payload.txt";
    fs::write(host_file, b"CLI Network Mode Test Content").unwrap();

    // Put via CLI --network
    let res = Command::new(bin)
        .args([
            "--network",
            "--image",
            img_path,
            "put",
            host_file,
            "remote_net.txt",
        ])
        .status()
        .expect("Failed to run put");
    assert!(res.success());

    // Ls via CLI --network
    let output = Command::new(bin)
        .args(["--network", "--image", img_path, "ls"])
        .output()
        .expect("Failed to run ls");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("remote_net.txt"));

    let _ = fs::remove_file(host_file);
    let _ = fs::remove_file(img_path);
}
