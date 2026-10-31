//! Retry policy for failed model requests.
use crate::model::ModelError;
use std::time::Duration;

/// Retries after the first failed attempt.
pub(crate) const MAX_RETRIES: u32 = 5;
/// The longest provider-requested delay we wait out; longer waits fail the
/// request so the user can decide.
pub(crate) const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);
const BASE_DELAY: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(32);

/// How long to wait before retry `attempt` (1-based) of a request that
/// failed with `error`, or `None` when it must not be retried.
pub(crate) fn delay(error: &ModelError, attempt: u32) -> Option<Duration> {
    if !error.retryable() || attempt > MAX_RETRIES {
        return None;
    }
    match error.retry_after() {
        Some(wait) if wait > MAX_RETRY_AFTER => None,
        Some(wait) => Some(wait),
        None => Some(backoff(attempt)),
    }
}

/// Exponential backoff with ±25 % jitter, so parallel clients spread out.
fn backoff(attempt: u32) -> Duration {
    let base = BASE_DELAY
        .saturating_mul(1 << attempt.saturating_sub(1).min(6))
        .min(MAX_BACKOFF);
    let jitter = (uuid::Uuid::new_v4().as_u128() % 501) as f64 / 1000.0 - 0.25;
    base.mul_f64(1.0 + jitter)
}

#[cfg(test)]
mod tests;
