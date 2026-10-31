use super::*;
#[test]
fn reused_daemon_id_replaces_old_route_and_active_routes_stay_bounded() {
    let mut ids = VecDeque::new();
    assert_eq!(remember(&mut ids, "old".into(), 12), None);
    assert_eq!(remember(&mut ids, "new".into(), 12), None);
    assert_eq!(ids.len(), 1);
    assert_eq!(ids[0].0, "new");
    for id in 13..180 {
        remember(&mut ids, id.to_string(), id);
    }
    assert_eq!(ids.len(), 128);
}
