use super::*;
use tokio::io::BufReader;

#[tokio::test]
async fn framing_handles_multiple_messages_and_rejects_partial_or_oversize() {
    let mut input = BufReader::new(&b"{\"a\":1}\n{\"b\":2}\n"[..]);
    assert_eq!(
        read_frame(&mut input).await.unwrap().unwrap(),
        br#"{"a":1}"#
    );
    assert_eq!(
        read_frame(&mut input).await.unwrap().unwrap(),
        br#"{"b":2}"#
    );
    assert!(read_frame(&mut input).await.unwrap().is_none());
    assert!(read_frame(&mut BufReader::new(&b"{}"[..])).await.is_err());
    let bytes = vec![b'x'; MAX_FRAME_BYTES + 1];
    assert!(
        read_frame(&mut BufReader::new(bytes.as_slice()))
            .await
            .is_err()
    );
}
