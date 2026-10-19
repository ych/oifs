//! Tests for IPC frame framing: pooled buffers, chunked payload streaming and
//! bounded frame size enforcement (P4.8).

use oifs::disk::CompressionMode;
use oifs::filters::FilterConfig;
use oifs::ipc::{IpcRequest, IpcResponse, MAX_FRAME_BYTES, read_framed, write_framed};
use std::io::Cursor;

#[test]
fn test_frame_roundtrip_small_payload() {
    let mut buf = Vec::new();
    write_framed(&mut buf, &IpcRequest::Ping).unwrap();
    let req: IpcRequest = read_framed(&mut Cursor::new(buf)).unwrap();
    assert!(matches!(req, IpcRequest::Ping));

    // And the reverse direction (response frame).
    let mut buf = Vec::new();
    write_framed(
        &mut buf,
        &IpcResponse::Success(oifs::ipc::IpcResponseData::Pong),
    )
    .unwrap();
    let resp: IpcResponse = read_framed(&mut Cursor::new(buf)).unwrap();
    match resp {
        IpcResponse::Success(oifs::ipc::IpcResponseData::Pong) => {}
        other => panic!("unexpected response: {other:?}"),
    }
}

#[test]
fn test_frame_roundtrip_large_payload_streams_in_chunks() {
    // Larger than the 64 KiB streaming chunk so the chunked read path is used.
    let payload = vec![0xABu8; 256 * 1024];
    let mut buf = Vec::new();
    write_framed(
        &mut buf,
        &IpcRequest::WriteData {
            inode_id: 42,
            file_offset: 0,
            data: payload.clone(),
            compression_mode: CompressionMode::Never,
            filter_config: FilterConfig::default(),
        },
    )
    .unwrap();

    let req: IpcRequest = read_framed(&mut Cursor::new(buf)).unwrap();
    match req {
        IpcRequest::WriteData { inode_id, data, .. } => {
            assert_eq!(inode_id, 42);
            assert_eq!(data, payload);
        }
        other => panic!("unexpected request: {other:?}"),
    }
}

#[test]
fn test_frame_roundtrip_repeated_reuses_pool() {
    // Repeated frames must not corrupt each other: the pooled buffers are
    // cleared and reused between messages.
    let mut stream = Vec::new();
    for i in 0..16u64 {
        write_framed(&mut stream, &IpcRequest::ReadInode { inode_id: i }).unwrap();
    }
    let mut cursor = Cursor::new(stream);
    for i in 0..16u64 {
        let req: IpcRequest = read_framed(&mut cursor).unwrap();
        match req {
            IpcRequest::ReadInode { inode_id } => assert_eq!(inode_id, i),
            other => panic!("unexpected request: {other:?}"),
        }
    }
}

#[test]
fn test_oversized_frame_is_rejected_before_allocation() {
    // Announce a frame larger than the limit; the reader must reject it
    // instead of allocating the announced size.
    let mut raw = Vec::new();
    let oversized = (MAX_FRAME_BYTES as u64 + 1) as u32;
    raw.extend_from_slice(&oversized.to_le_bytes());
    raw.extend_from_slice(&[0u8; 16]);

    let err = read_framed::<_, IpcRequest>(&mut Cursor::new(raw)).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn test_truncated_frame_is_an_error() {
    let mut buf = Vec::new();
    write_framed(&mut buf, &IpcRequest::Ping).unwrap();
    buf.truncate(buf.len() - 2);
    assert!(read_framed::<_, IpcRequest>(&mut Cursor::new(buf)).is_err());
}
