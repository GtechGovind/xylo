use super::*;
use serde_json::json;
use tokio::io::BufReader;
#[tokio::test]
async fn unicode_byte_lengths_and_back_to_back_frames_round_trip() {
    let (mut input, output) = tokio::io::duplex(4096);
    let first = json!({"jsonrpc":"2.0","id":1,"result":"世界 🦀"});
    let second = json!({"jsonrpc":"2.0","method":"initialized","params":{}});
    let a = first.clone();
    let b = second.clone();
    let writer = tokio::spawn(async move {
        write_frame(&mut input, &a).await.unwrap();
        write_frame(&mut input, &b).await.unwrap();
    });
    let mut reader = BufReader::new(output);
    assert_eq!(read_frame(&mut reader).await.unwrap(), Some(first));
    assert_eq!(read_frame(&mut reader).await.unwrap(), Some(second));
    writer.await.unwrap();
    assert!(read_frame(&mut reader).await.unwrap().is_none());
}
#[tokio::test]
async fn oversized_or_ambiguous_headers_fail_before_allocating_a_body() {
    for bytes in [
        b"Content-Length: 999999999\r\n\r\n".as_slice(),
        b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}".as_slice(),
        b"Content-Length: 2\n\n{}".as_slice(),
    ] {
        assert!(read_frame(&mut BufReader::new(bytes)).await.is_err());
    }
    let bytes = vec![b'x'; MAX_HEADER + 1];
    assert!(
        read_frame(&mut BufReader::new(bytes.as_slice()))
            .await
            .is_err()
    );
}
