use super::*;
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncWriteExt, ReadBuf};

const PREFIX: &[u8] = b"retained native output";

fn lease(directory: &Path) -> Arc<RunLease> {
    let operation = File::create(directory.join("operation.lock")).unwrap();
    let profile = File::create(directory.join("profile.lock")).unwrap();
    fs2::FileExt::lock_exclusive(&operation).unwrap();
    fs2::FileExt::lock_exclusive(&profile).unwrap();
    RunLease::new(directory.to_owned(), operation, profile).unwrap()
}

async fn observed(reader: &Reader, bytes: u64) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if reader.state.lock().unwrap().observed >= bytes {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Reader must consume the explicit readiness prefix");
}

#[tokio::test]
async fn held_open_pipe_preserves_known_end_cause_and_inexact_prefix() {
    for interruption in [
        Some(EnvironmentCaptureReason::Cancelled),
        Some(EnvironmentCaptureReason::TimedOut),
        None,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("output");
        let (mut input, pipe) = tokio::io::duplex(128);
        let reader = Reader::spawn(
            Some(pipe),
            File::create(&output).unwrap(),
            4,
            lease(directory.path()),
        );
        input.write_all(PREFIX).await.unwrap();
        observed(&reader, PREFIX.len() as u64).await;
        assert_eq!(active(directory.path()), 1);
        let result = reader
            .finish_with_timeout(interruption, Duration::from_millis(20))
            .await;
        // The writer is deliberately still open: no EOF was observed.
        assert!(!result.observed_exact);
        assert_ne!(result.reason, EnvironmentCaptureReason::Eof);
        assert_eq!(
            result.reason,
            interruption.unwrap_or(EnvironmentCaptureReason::Interrupted)
        );
        assert_eq!(result.observed, PREFIX.len() as u64);
        assert_eq!(result.preview, PREFIX[..4]);
        assert_eq!(std::fs::read(output).unwrap(), PREFIX);
        assert_eq!(active(directory.path()), 0);
        let operation = File::open(directory.path().join("operation.lock")).unwrap();
        fs2::FileExt::try_lock_exclusive(&operation).unwrap();
        drop(input);
    }
}

#[tokio::test]
async fn observed_eof_is_exact_and_complete_without_command_interruption() {
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("output");
    let (mut input, pipe) = tokio::io::duplex(128);
    let reader = Reader::spawn(
        Some(pipe),
        File::create(&output).unwrap(),
        128,
        lease(directory.path()),
    );
    input.write_all(PREFIX).await.unwrap();
    drop(input);
    let result = reader.finish(None).await;
    assert!(result.observed_exact);
    assert_eq!(result.reason, EnvironmentCaptureReason::Eof);
    assert_eq!(std::fs::read(output).unwrap(), PREFIX);
    assert_eq!(active(directory.path()), 0);
}

struct ReadFailure(bool);
impl AsyncRead for ReadFailure {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.0 {
            Poll::Ready(Err(io::Error::other("controlled pipe read failure")))
        } else {
            self.0 = true;
            output.put_slice(PREFIX);
            Poll::Ready(Ok(()))
        }
    }
}

#[tokio::test]
async fn explicit_read_error_takes_precedence_over_command_cancellation() {
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("output");
    let reader = Reader::spawn(
        Some(ReadFailure(false)),
        File::create(&output).unwrap(),
        128,
        lease(directory.path()),
    );
    let result = reader
        .finish(Some(EnvironmentCaptureReason::Cancelled))
        .await;
    assert_eq!(result.reason, EnvironmentCaptureReason::ReadError);
    assert!(!result.observed_exact);
    assert_eq!(std::fs::read(output).unwrap(), PREFIX);
}

#[tokio::test]
async fn actual_write_error_takes_precedence_over_command_timeout() {
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("output");
    std::fs::write(&output, b"original").unwrap();
    let (mut input, pipe) = tokio::io::duplex(128);
    let reader = Reader::spawn(
        Some(pipe),
        File::open(&output).unwrap(),
        128,
        lease(directory.path()),
    );
    input.write_all(PREFIX).await.unwrap();
    drop(input);
    let result = reader
        .finish(Some(EnvironmentCaptureReason::TimedOut))
        .await;
    assert_eq!(result.reason, EnvironmentCaptureReason::WriteError);
    assert_eq!(std::fs::read(output).unwrap(), b"original");
    assert_eq!(active(directory.path()), 0);
}

#[tokio::test]
async fn unexpected_reader_join_error_is_a_read_error() {
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("output");
    let (mut input, pipe) = tokio::io::duplex(128);
    let reader = Reader::spawn(
        Some(pipe),
        File::create(&output).unwrap(),
        128,
        lease(directory.path()),
    );
    input.write_all(PREFIX).await.unwrap();
    observed(&reader, PREFIX.len() as u64).await;
    reader.task.abort();
    let result = reader
        .finish(Some(EnvironmentCaptureReason::Cancelled))
        .await;
    assert_eq!(result.reason, EnvironmentCaptureReason::ReadError);
    assert!(!result.observed_exact);
    assert_eq!(std::fs::read(output).unwrap(), PREFIX);
}

#[tokio::test]
async fn retained_byte_limit_takes_precedence_over_command_cancellation() {
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("output");
    let (mut input, pipe) = tokio::io::duplex(16 * 1024);
    let reader = Reader::spawn(
        Some(pipe),
        File::create(&output).unwrap(),
        4,
        lease(directory.path()),
    );
    let writer = tokio::spawn(async move {
        let chunk = [b'x'; 16 * 1024];
        for _ in 0..STREAM_LIMIT / chunk.len() as u64 {
            input.write_all(&chunk).await.unwrap();
        }
        input.write_all(b"!").await.unwrap();
    });
    tokio::time::timeout(Duration::from_secs(10), writer)
        .await
        .unwrap()
        .unwrap();
    let result = reader
        .finish(Some(EnvironmentCaptureReason::Cancelled))
        .await;
    assert_eq!(result.reason, EnvironmentCaptureReason::ByteLimit);
    assert!(result.observed_exact);
    assert_eq!(result.observed, STREAM_LIMIT + 1);
    assert_eq!(std::fs::metadata(output).unwrap().len(), STREAM_LIMIT);
    assert_eq!(active(directory.path()), 0);
}
