//! Tests for the IPC transport hardening (SEC-18):
//! challenge-response authentication, socket/secret permissions and the
//! bounds on unauthenticated peers.

use oifs::ipc::{
    AuthCredential, IpcClient, IpcRequest, IpcResponse, IpcResponseData, MasterOrClient,
    SessionMode, bind_or_connect, get_socket_path, get_token_path, read_framed, write_framed,
};
use oifs::session::OifsSession;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

struct TestImage {
    path: String,
}

impl TestImage {
    fn new(name: &str) -> Self {
        let path = format!("target/{}_{}.img", name, std::process::id());
        if Path::new(&path).exists() {
            let _ = fs::remove_file(&path);
        }
        Self { path }
    }
}

impl Drop for TestImage {
    fn drop(&mut self) {
        if Path::new(&self.path).exists() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[test]
fn test_socket_and_token_are_owner_only() {
    let img = TestImage::new("test_sec18_socket_perms");
    let _master = OifsSession::open_ipc(&img.path, 10 * 1024 * 1024).expect("open ipc master");

    let socket_path = get_socket_path(&img.path);
    let token_path = get_token_path(&img.path);

    assert!(socket_path.exists(), "the socket must exist");
    assert!(token_path.exists(), "the session secret must exist");

    let socket_mode = fs::metadata(&socket_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        socket_mode, 0o600,
        "the socket must be owner-only: {socket_mode:o}"
    );

    let token_mode = fs::metadata(&token_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        token_mode, 0o600,
        "the secret must be owner-only: {token_mode:o}"
    );

    // The socket lives in a private directory, not in the world-writable /tmp.
    let parent = socket_path.parent().unwrap();
    let dir_mode = fs::metadata(parent).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        dir_mode, 0o700,
        "the socket directory must be private: {dir_mode:o}"
    );
}

#[test]
fn test_rendezvous_file_is_owner_only() {
    let img = TestImage::new("test_sec18_rendezvous_perms");
    let _master =
        OifsSession::open_network(&img.path, 10 * 1024 * 1024, None).expect("open network master");

    let rendezvous = oifs::ipc::get_master_info_path(&img.path);
    assert!(rendezvous.exists(), "the rendezvous file must exist");

    let mode = fs::metadata(&rendezvous).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "the rendezvous file must be owner-only: {mode:o}"
    );

    // It carries the session secret, so the content must not be a bare
    // address any more.
    let content = fs::read_to_string(&rendezvous).unwrap();
    let info: oifs::ipc::MasterInfo = serde_json::from_str(&content).unwrap();
    assert_eq!(info.auth_token.len(), 64, "the secret must be published");
}

#[test]
fn test_client_rejects_a_master_that_cannot_prove_its_key() {
    let _img = TestImage::new("test_sec18_fake_master");

    // A "fake master" that answers the challenge with a wrong MAC.
    let listener = std::os::unix::net::UnixListener::bind("/tmp/oifs_fake_master_test.sock")
        .expect("bind fake master");
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        // The client's challenge.
        let _ = read_framed::<_, IpcRequest>(&mut stream);
        let response = IpcResponse::Success(IpcResponseData::AuthChallenge {
            server_nonce: [7u8; 32],
            salt: [0u8; 16],
            encrypted: false,
        });
        let _ = write_framed(&mut stream, &response);
        // The client's answer, then a bogus verification.
        let _ = read_framed::<_, IpcRequest>(&mut stream);
        let bogus = IpcResponse::Success(IpcResponseData::AuthVerify {
            server_mac: [0u8; 32],
        });
        let _ = write_framed(&mut stream, &bogus);
    });

    let client = IpcClient::new(
        oifs::ipc::IpcStream::Unix(
            std::os::unix::net::UnixStream::connect("/tmp/oifs_fake_master_test.sock").unwrap(),
        ),
        std::path::PathBuf::from("/tmp/oifs_fake_master_test.sock"),
    );

    let err = client
        .authenticate(&AuthCredential::Secret([1u8; 32]))
        .expect_err("a fake master must be rejected");
    assert!(
        err.to_string().contains("failed to prove"),
        "unexpected error: {err}"
    );

    handle.join().unwrap();
    let _ = fs::remove_file("/tmp/oifs_fake_master_test.sock");
}

#[test]
fn test_unauthenticated_peer_cannot_read_data() {
    let img = TestImage::new("test_sec18_unauthenticated");
    let master = OifsSession::open_ipc(&img.path, 10 * 1024 * 1024).expect("open ipc master");
    let root = master.resolve_path(".").unwrap();
    let fid = master.create_file(root, "secret.txt").unwrap();
    master
        .write_data(fid, 0, b"top secret", oifs::disk::CompressionMode::Never)
        .unwrap();

    // A rogue peer that connects directly and skips the handshake.
    let socket_path = get_socket_path(&img.path);
    let mut stream = std::os::unix::net::UnixStream::connect(&socket_path).unwrap();
    write_framed(&mut stream, &IpcRequest::ReadData { inode_id: fid }).unwrap();
    let response: IpcResponse = read_framed(&mut stream).unwrap();
    match response {
        IpcResponse::Error(msg) => {
            assert!(
                msg.contains("Password required"),
                "unexpected error message: {msg}"
            );
        }
        other => panic!("an unauthenticated peer must not read data: {other:?}"),
    }
}

#[test]
fn test_client_authenticates_with_the_published_secret() {
    let img = TestImage::new("test_sec18_secret_auth");
    let _master = OifsSession::open_ipc(&img.path, 10 * 1024 * 1024).expect("open ipc master");

    // A second session becomes a client and must authenticate with the
    // secret published by the master.
    let client = OifsSession::open_ipc(&img.path, 0).expect("open ipc client");
    assert!(!client.is_direct());
    // The root inode is 0; the point is that the request is served at all.
    let root = client.resolve_path(".").unwrap();
    assert_eq!(root, 0);
}

#[test]
fn test_master_and_client_agree_on_the_session_secret() {
    let img = TestImage::new("test_sec18_secret_match");
    let _master = OifsSession::open_ipc(&img.path, 10 * 1024 * 1024).expect("open ipc master");

    // The client obtains the secret from the token file ...
    let token = fs::read(get_token_path(&img.path)).unwrap();
    assert_eq!(token.len(), 32);

    // ... and the master accepts a client that uses exactly that secret.
    match bind_or_connect(&img.path, &SessionMode::Local).expect("connect") {
        MasterOrClient::Client {
            stream,
            target_path,
            auth_secret,
        } => {
            let secret = auth_secret.expect("the master must publish a secret");
            let client = IpcClient::new(stream, target_path);
            client
                .authenticate(&AuthCredential::Secret(secret))
                .expect("authentication with the published secret must succeed");
        }
        MasterOrClient::Master(_) => panic!("expected to connect as a client"),
    }
}
