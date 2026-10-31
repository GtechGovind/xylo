use super::poll_delay;
use std::time::Duration;

#[test]
fn status_polling_backs_off_after_errors_and_stays_bounded() {
    assert_eq!(poll_delay(0), Duration::from_millis(800));
    assert_eq!(poll_delay(1), Duration::from_millis(1600));
    assert_eq!(poll_delay(10), Duration::from_secs(30));
    assert_eq!(poll_delay(u32::MAX), Duration::from_secs(30));
}
