use super::*;

#[test]
fn transient_errors_back_off_exponentially_with_jitter_and_stop_after_the_limit() {
    let error = ModelError::transient("reset");
    let first = delay(&error, 1).unwrap();
    assert!(first >= Duration::from_millis(750) && first <= Duration::from_millis(1250));
    let fourth = delay(&error, 4).unwrap();
    assert!(fourth >= Duration::from_secs(6) && fourth <= Duration::from_secs(10));
    assert!(delay(&error, MAX_RETRIES).is_some());
    assert!(delay(&error, MAX_RETRIES + 1).is_none());
}

#[test]
fn retry_after_is_honoured_up_to_two_minutes() {
    let limited = |seconds| ModelError::RateLimited {
        retry_after: Some(Duration::from_secs(seconds)),
        message: "429".into(),
    };
    assert_eq!(delay(&limited(60), 1), Some(Duration::from_secs(60)));
    assert_eq!(delay(&limited(120), 1), Some(Duration::from_secs(120)));
    assert_eq!(delay(&limited(121), 1), None);
}

#[test]
fn permanent_failures_are_not_retried() {
    for error in [
        ModelError::Authentication("401".into()),
        ModelError::InvalidRequest("bad".into()),
        ModelError::ContextOverflow("full".into()),
        ModelError::Interrupted,
        ModelError::permanent("refused"),
    ] {
        assert_eq!(delay(&error, 1), None);
    }
}
