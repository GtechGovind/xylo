use super::{ActionReviewSummary, Entry, FORM_CACHE, STUCK_BUSY, action_stuck, evictable_forms};
use std::{collections::BTreeMap, path::PathBuf, time::Instant};
use uuid::Uuid;

fn item(busy: bool, blocked: bool) -> ActionReviewSummary {
    ActionReviewSummary {
        id: Uuid::new_v4(),
        turn_id: Uuid::new_v4(),
        name: "shell_command".into(),
        writes: true,
        state: "outcome_unknown".into(),
        updated_at: 0,
        revision: "r1".into(),
        busy,
        blocked_reason: blocked.then(|| "Execution lease is unavailable".into()),
    }
}

#[test]
fn blocked_actions_are_stuck_immediately_and_busy_ones_after_the_timeout() {
    let now = Instant::now();
    let started = now.checked_sub(STUCK_BUSY).unwrap();
    assert!(action_stuck(&item(false, true), None, now));
    assert!(!action_stuck(&item(true, false), None, now));
    assert!(!action_stuck(&item(true, false), Some(&now), now));
    assert!(action_stuck(&item(true, false), Some(&started), now));
    assert!(!action_stuck(&item(false, false), Some(&started), now));
}

#[test]
fn a_full_cache_of_unsaved_forms_never_refuses_a_new_chat() {
    let dirty = |index: u128| {
        let key = (Uuid::from_u128(index), PathBuf::from("/project"));
        let entry = Entry {
            dirty: true,
            ..Entry::default()
        };
        (key, entry)
    };
    let entries: BTreeMap<_, _> = (0..40).map(dirty).collect();
    assert!(
        evictable_forms(&entries).is_empty(),
        "Unsaved notes are kept even past the bound"
    );
    let settled: BTreeMap<_, _> = (0..40u128)
        .map(|index| {
            (
                (Uuid::from_u128(index), PathBuf::from("/project")),
                Entry::default(),
            )
        })
        .collect();
    assert_eq!(evictable_forms(&settled).len(), 40 + 1 - FORM_CACHE);
}
