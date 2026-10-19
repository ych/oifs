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

#[test]
fn test_crafted_length_prefix_cannot_trigger_huge_allocation() {
    // SEC-17: a tiny frame whose `Vec<u8>` length prefix claims `u64::MAX`
    // must be rejected by the deserializer's allocation limit instead of
    // making the process allocate gigabytes.
    //
    // The variant index is taken from a real encoding, so the test does not
    // depend on the order of the `IpcRequest` variants.
    let mut probe = Vec::new();
    write_framed(
        &mut probe,
        &IpcRequest::WriteData {
            inode_id: 1,
            file_offset: 0,
            data: vec![0u8; 4],
            compression_mode: CompressionMode::Never,
            filter_config: FilterConfig::default(),
        },
    )
    .unwrap();
    // probe = [len: u32][payload]; the payload starts with the variant index.
    let variant = u32::from_le_bytes(probe[4..8].try_into().unwrap());

    // `IpcRequest::WriteData` is `inode_id: u64, file_offset: u64, data: Vec<u8>`,
    // so the length prefix sits at byte offset 20 of the payload.
    let mut payload = Vec::new();
    payload.extend_from_slice(&variant.to_le_bytes());
    payload.extend_from_slice(&1u64.to_le_bytes()); // inode_id
    payload.extend_from_slice(&0u64.to_le_bytes()); // file_offset
    payload.extend_from_slice(&u64::MAX.to_le_bytes()); // data length: absurd

    // Frame layout: [len: u32][payload]
    let mut raw = Vec::new();
    raw.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    raw.extend_from_slice(&payload);

    let err = read_framed::<_, IpcRequest>(&mut Cursor::new(raw)).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}
