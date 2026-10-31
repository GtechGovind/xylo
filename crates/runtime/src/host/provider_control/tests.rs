use super::*;
#[test]
fn next_enqueued_setup_cancels_older_work_and_invalidates_queued_catalogs() {
    let control = DiscoveryControl::default();
    let old = control.begin();
    let old_cancel = control.token(old).unwrap();
    let new = control.begin();
    assert_ne!(old, new);
    assert!(old_cancel.is_cancelled());
    assert!(control.token(old).is_none());
    assert_eq!(control.generation(), new);
    assert!(!control.token(new).unwrap().is_cancelled());
}
