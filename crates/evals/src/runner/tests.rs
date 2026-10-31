use super::*;

const SHORT: Duration = Duration::from_millis(50);

fn started() -> EngineEvent {
    EngineEvent::Started
}

#[tokio::test]
async fn a_stream_that_closes_in_time_is_neither_timed_out_nor_hung() {
    let (sender, mut incoming) = mpsc::channel(4);
    sender.send(started()).await.unwrap();
    drop(sender);
    let cancel = CancellationToken::new();
    let mut seen = 0;
    let ending = collect_events(&mut incoming, SHORT, SHORT, &cancel, |_| seen += 1).await;
    assert_eq!(ending, Ending::default());
    assert_eq!(seen, 1);
    assert!(!cancel.is_cancelled());
}

#[tokio::test]
async fn a_stream_that_closes_after_cancellation_is_only_timed_out() {
    let (sender, mut incoming) = mpsc::channel(4);
    let cancel = CancellationToken::new();
    let closer = cancel.clone();
    tokio::spawn(async move {
        closer.cancelled().await;
        drop(sender);
    });
    let ending = collect_events(
        &mut incoming,
        SHORT,
        Duration::from_secs(5),
        &cancel,
        |_| {},
    )
    .await;
    assert_eq!(
        ending,
        Ending {
            timed_out: true,
            hung: false
        }
    );
}

#[tokio::test]
async fn a_stream_left_open_after_cancellation_is_hung_after_the_grace() {
    let (sender, mut incoming) = mpsc::channel::<EngineEvent>(4);
    let cancel = CancellationToken::new();
    let started = std::time::Instant::now();
    let ending = collect_events(&mut incoming, SHORT, SHORT, &cancel, |_| {}).await;
    assert_eq!(
        ending,
        Ending {
            timed_out: true,
            hung: true
        }
    );
    assert!(cancel.is_cancelled());
    assert!(started.elapsed() >= SHORT * 2);
    drop(sender);
}
